import NIOCore
import NIOSSH

/// Reads on demand, so a slow consumer can use SSH's receive window.
final class BridgeLines: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = SSHChannelData

    // Leave room for an 8 MiB image encoded as base64 and its JSON envelope.
    static let maxLineBytes = 16 * 1024 * 1024

    private let command: BridgeCommand
    private var ready: EventLoopPromise<Void>?
    private var buffer: ByteBuffer
    private var scanned = 0
    private var channel: Channel?
    private var pending: EventLoopPromise<String?>?
    private var reading = false
    private var ended = false
    private var failure: Error?

    init(command: BridgeCommand, ready: EventLoopPromise<Void>, allocator: ByteBufferAllocator) {
        self.command = command
        self.ready = ready
        self.buffer = allocator.buffer(capacity: 0)
    }

    func handlerAdded(context: ChannelHandlerContext) { channel = context.channel }

    func channelActive(context: ChannelHandlerContext) {
        let event = SSHChannelRequestEvent.ExecRequest(command: command.rawValue, wantReply: true)
        let sent = context.eventLoop.makePromise(of: Void.self)
        sent.futureResult.whenFailure { self.end($0) }
        context.triggerUserOutboundEvent(event, promise: sent)
        context.fireChannelActive()
    }

    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        switch event {
        case is ChannelSuccessEvent:
            ready?.succeed(())
            ready = nil
        case is ChannelFailureEvent:
            end(SSHFailure.commandRefused)
            context.close(promise: nil)
        default:
            context.fireUserInboundEventTriggered(event)
        }
    }

    func readLine(_ promise: EventLoopPromise<String?>) {
        guard pending == nil else {
            promise.fail(SSHFailure.concurrentRead)
            return
        }
        pending = promise
        deliverLine()
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let frame = unwrapInboundIn(data)
        // Stderr is separate from mobile API JSON.
        guard frame.type == .channel, case .byteBuffer(var bytes) = frame.data else { return }
        buffer.writeBuffer(&bytes)
        deliverLine()
    }

    func channelReadComplete(context: ChannelHandlerContext) {
        reading = false
        deliverLine()
    }

    func channelInactive(context: ChannelHandlerContext) {
        end(nil)
        channel = nil
        context.fireChannelInactive()
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        end(error)
        context.close(promise: nil)
    }

    func close() {
        buffer.clear()
        scanned = 0
        end(nil)
    }

    private func deliverLine() {
        guard let pending else { return }
        let readable = buffer.readableBytesView
        if let end = readable.dropFirst(scanned).firstIndex(of: 10) {
            let length = end - buffer.readerIndex
            guard length <= Self.maxLineBytes else {
                failLine(SSHFailure.lineTooLong)
                return
            }
            var bytes = buffer.readSlice(length: length)!.readableBytesView
            if bytes.last == 13 { bytes = bytes.dropLast() }
            buffer.moveReaderIndex(forwardBy: 1)
            scanned = 0
            buffer.discardReadBytes()
            self.pending = nil
            if let line = String(validating: bytes, as: UTF8.self) {
                pending.succeed(line)
            } else {
                pending.fail(SSHFailure.invalidLine)
                channel?.close(promise: nil)
            }
        } else if readable.count > Self.maxLineBytes {
            failLine(SSHFailure.lineTooLong)
        } else if ended {
            self.pending = nil
            if let failure {
                pending.fail(failure)
            } else if readable.isEmpty {
                pending.succeed(nil)
            } else {
                pending.fail(SSHFailure.invalidLine)
            }
        } else {
            scanned = readable.count
            if !reading {
                reading = true
                channel?.read()
            }
        }
    }

    private func failLine(_ error: SSHFailure) {
        pending?.fail(error)
        pending = nil
        channel?.close(promise: nil)
    }

    private func end(_ error: Error?) {
        guard !ended else { return }
        ended = true
        failure = error
        ready?.fail(error ?? SSHFailure.closed)
        ready = nil
        deliverLine()
    }
}
