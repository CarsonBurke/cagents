import CagentsMobileShared
import CagentsSSH
import Foundation

public enum NativeTransport {
    public static func mobile(
        endpoint: SSHEndpoint, phoneKey: PhoneKey
    ) -> NativeBridgeTransport {
        NativeBridgeTransport(dialer: Dialer(host: endpoint.host) {
            try await SSHConnection.openMobile(endpoint, phoneKey: phoneKey)
        })
    }

    public static func pairing(code: PairingCode, host: String) -> NativeBridgeTransport {
        let bytes = code.seed
        let seed = Data((0..<bytes.size).map { UInt8(bitPattern: bytes.get(index: $0)) })
        let endpoint = SSHEndpoint(host: host, port: Int(code.port), user: code.user, fingerprint: code.hostKey)
        return NativeBridgeTransport(dialer: Dialer(host: host) {
            try await SSHConnection.openPairing(endpoint, seed: seed)
        })
    }
}

private final class Dialer: NSObject, NativeSshDialer {
    private let host: String
    private let connect: @Sendable () async throws -> SSHConnection

    init(host: String, connect: @escaping @Sendable () async throws -> SSHConnection) {
        self.host = host
        self.connect = connect
    }

    func open(completion: NativeSshOpening) -> any NativeSshOperation {
        let task = Task {
            do {
                let connection = try await connect()
                completion.opened(channel: SshLineChannel(connection: connection))
            } catch SSHFailure.hostKeyChanged(let fingerprint, let pinned) {
                completion.hostKeyChanged(fingerprint: fingerprint, pinned: pinned, host: host)
            } catch SSHFailure.unauthorized {
                completion.unauthorized(message: "The computer refused this phone's key.")
            } catch SSHFailure.commandRefused {
                completion.refused(message: "The computer refused to start the cagents bridge.")
            } catch SSHFailure.unexpectedHostKeyType {
                completion.refused(message: "The computer did not present the host key type required for pairing.")
            } catch {
                completion.network(message: error.localizedDescription)
            }
        }
        return Operation(task: task)
    }
}

private final class Operation: NSObject, NativeSshOperation {
    let task: Task<Void, Never>

    init(task: Task<Void, Never>) { self.task = task }

    func cancel() { task.cancel() }
}

private final class SshLineChannel: NSObject, NativeSshChannel {
    let connection: SSHConnection

    init(connection: SSHConnection) { self.connection = connection }

    func read(completion: NativeSshReading) {
        Task {
            do {
                let line = try await connection.readLine()
                completion.received(line: line)
            } catch {
                completion.failed(message: error.localizedDescription)
            }
        }
    }

    func write(line: String, completion: NativeSshWriting) {
        Task {
            do {
                try await connection.writeLine(line)
                completion.written()
            } catch {
                completion.failed(message: error.localizedDescription)
            }
        }
    }

    func close() { connection.close() }
}
