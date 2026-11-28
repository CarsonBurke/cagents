import Crypto
import Foundation
import NIOCore
import NIOPosix
import NIOSSH

public enum SSHFailure: Error {
    case hostKeyChanged(fingerprint: String, pinned: String)
    case unexpectedHostKeyType
    case unauthorized
    case commandRefused
    case timedOut
    case closed
    case invalidLine
    case lineTooLong
    case concurrentRead
}

package enum BridgeCommand: String, Sendable {
    // OpenSSH's forced command supplies the private directory after --pair.
    case pair = "cagents daemon bridge --pair"
    case mobile = "cagents daemon bridge --mobile"
}

public struct SSHEndpoint: Sendable {
    public let host: String
    public let port: Int
    public let user: String
    public let fingerprint: String

    public init(host: String, port: Int, user: String, fingerprint: String) {
        self.host = host
        self.port = port
        self.user = user
        self.fingerprint = fingerprint
    }
}

public final class SSHConnection: Sendable {
    private static let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)

    private let parent: Channel
    private let channel: Channel
    private let lines: BridgeLines

    private init(parent: Channel, channel: Channel, lines: BridgeLines) {
        self.parent = parent
        self.channel = channel
        self.lines = lines
    }

    #if os(iOS)
    public static func openMobile(_ endpoint: SSHEndpoint, phoneKey: PhoneKey) async throws -> SSHConnection {
        try await open(endpoint, key: phoneKey.ssh, command: .mobile)
    }

    public static func openPairing(_ endpoint: SSHEndpoint, seed: Data) async throws -> SSHConnection {
        try await open(endpoint, key: SSHKey.pairing(seed: seed), command: .pair)
    }
    #endif

    package static func open(
        _ endpoint: SSHEndpoint,
        key: SSHKey,
        command: BridgeCommand
    ) async throws -> SSHConnection {
        let loop = group.next()
        let completion = loop.makePromise(of: SSHConnection.self)
        let startup = ConnectionStartup(completion)
        let parent: Channel
        do {
            parent = try await ClientBootstrap(group: loop)
                .connectTimeout(.seconds(8))
                .channelInitializer { channel in
                    channel.eventLoop.makeCompletedFuture {
                        let handler = NIOSSHHandler(
                            role: .client(.init(
                                userAuthDelegate: PublicKeyAuth(user: endpoint.user, key: key.key),
                                serverAuthDelegate: PinnedHostKey(endpoint: endpoint, ed25519Only: command == .pair)
                            )),
                            allocator: channel.allocator,
                            inboundChildChannelInitializer: { _, _ in
                                channel.eventLoop.makeFailedFuture(SSHFailure.commandRefused)
                            }
                        )
                        try channel.pipeline.syncOperations.addHandlers(handler, CloseOnError(startup: startup))
                    }
                }
                .connect(host: endpoint.host, port: endpoint.port).get()
        } catch {
            loop.execute { startup.finish(.failure(error)) }
            throw error
        }

        return try await withTaskCancellationHandler {
            do {
                try Task.checkCancellation()
                let connection = try await openSession(
                    on: parent, command: command, startup: startup, result: completion.futureResult
                )
                try Task.checkCancellation()
                return connection
            } catch {
                parent.close(promise: nil)
                throw error
            }
        } onCancel: {
            parent.close(promise: nil)
        }
    }

    private static func openSession(
        on parent: Channel, command: BridgeCommand, startup: ConnectionStartup,
        result: EventLoopFuture<SSHConnection>
    ) async throws -> SSHConnection {
        let loop = parent.eventLoop
        let ready = loop.makePromise(of: Void.self)
        let lines = BridgeLines(command: command, ready: ready, allocator: parent.allocator)
        let timeout = loop.scheduleTask(in: .seconds(8)) {
            startup.finish(.failure(SSHFailure.timedOut))
            parent.close(promise: nil)
        }
        defer { timeout.cancel() }

        loop.execute {
            do {
                let ssh = try parent.pipeline.syncOperations.handler(type: NIOSSHHandler.self)
                let child = loop.makePromise(of: Channel.self)
                ssh.createChannel(child, channelType: .session) { channel, _ in
                    channel.setOption(ChannelOptions.autoRead, value: false).flatMap {
                        channel.pipeline.addHandler(lines)
                    }
                }
                child.futureResult.flatMap { channel in
                    channel.closeFuture.whenComplete { _ in parent.close(promise: nil) }
                    return ready.futureResult.map {
                        SSHConnection(parent: parent, channel: channel, lines: lines)
                    }
                }.whenComplete { startup.finish($0) }
            } catch {
                startup.finish(.failure(error))
            }
        }
        return try await result.get()
    }

    public func readLine() async throws -> String? {
        try await withTaskCancellationHandler {
            try Task.checkCancellation()
            let result = channel.eventLoop.makePromise(of: String?.self)
            channel.eventLoop.execute { self.lines.readLine(result) }
            return try await result.futureResult.get()
        } onCancel: {
            self.close()
        }
    }

    public func writeLine(_ line: String) async throws {
        guard !line.contains("\n"), !line.contains("\r") else { throw SSHFailure.invalidLine }
        guard line.utf8.count <= 1024 * 1024 else { throw SSHFailure.lineTooLong }
        try await withTaskCancellationHandler {
            try Task.checkCancellation()
            var data = channel.allocator.buffer(capacity: line.utf8.count + 1)
            data.writeString(line)
            data.writeInteger(UInt8(10))
            try await channel.writeAndFlush(SSHChannelData(type: .channel, data: .byteBuffer(data))).get()
        } onCancel: {
            self.close()
        }
    }

    public func close() {
        channel.eventLoop.execute {
            self.lines.close()
            self.parent.close(promise: nil)
        }
    }
}

private final class PublicKeyAuth: NIOSSHClientUserAuthenticationDelegate {
    private let user: String
    private let key: NIOSSHPrivateKey
    private var offered = false

    init(user: String, key: NIOSSHPrivateKey) {
        self.user = user
        self.key = key
    }

    func nextAuthenticationType(
        availableMethods: NIOSSHAvailableUserAuthenticationMethods,
        nextChallengePromise: EventLoopPromise<NIOSSHUserAuthenticationOffer?>
    ) {
        guard availableMethods.contains(.publicKey), !offered else {
            nextChallengePromise.fail(SSHFailure.unauthorized)
            return
        }
        offered = true
        nextChallengePromise.succeed(.init(username: user, serviceName: "ssh-connection",
                                           offer: .privateKey(.init(privateKey: key))))
    }
}

private struct PinnedHostKey: NIOSSHClientServerAuthenticationDelegate {
    let endpoint: SSHEndpoint
    let ed25519Only: Bool

    func validateHostKey(hostKey: NIOSSHPublicKey, validationCompletePromise: EventLoopPromise<Void>) {
        let parts = String(openSSHPublicKey: hostKey).split(separator: " ")
        guard parts.count == 2, let blob = Data(base64Encoded: String(parts[1])) else {
            validationCompletePromise.fail(SSHFailure.unexpectedHostKeyType)
            return
        }
        guard !ed25519Only || parts[0] == "ssh-ed25519" else {
            validationCompletePromise.fail(SSHFailure.unexpectedHostKeyType)
            return
        }
        let digest = Data(SHA256.hash(data: blob)).base64EncodedString().replacingOccurrences(of: "=", with: "")
        let fingerprint = "SHA256:" + digest
        guard fingerprint == endpoint.fingerprint else {
            validationCompletePromise.fail(SSHFailure.hostKeyChanged(fingerprint: fingerprint, pinned: endpoint.fingerprint))
            return
        }
        validationCompletePromise.succeed(())
    }
}

/// Only the parent channel's event loop resolves the handshake result.
private final class ConnectionStartup: @unchecked Sendable {
    private var promise: EventLoopPromise<SSHConnection>?

    init(_ promise: EventLoopPromise<SSHConnection>) { self.promise = promise }

    func finish(_ result: Result<SSHConnection, Error>) {
        guard let promise else { return }
        self.promise = nil
        promise.completeWith(result)
    }
}

private final class CloseOnError: ChannelInboundHandler {
    typealias InboundIn = Never
    let startup: ConnectionStartup

    init(startup: ConnectionStartup) { self.startup = startup }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        startup.finish(.failure(error))
        context.close(promise: nil)
    }

    func channelInactive(context: ChannelHandlerContext) {
        startup.finish(.failure(SSHFailure.closed))
        context.fireChannelInactive()
    }
}
