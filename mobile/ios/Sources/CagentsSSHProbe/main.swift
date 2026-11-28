import CagentsSSH
import Foundation

@main
struct Probe {
    static func main() async {
        do {
            try await run()
        } catch {
            FileHandle.standardError.write(Data(("\(error)\n").utf8))
            exit(EXIT_FAILURE)
        }
    }

    private static func run() async throws {
        let args = CommandLine.arguments
        if args.count == 4 && args[1] == "public-key" {
            let input = try Data(contentsOf: URL(fileURLWithPath: args[3]))
            let key = try fixtureKey(args[2], data: input)
            print(key.publicKey)
            return
        }
        guard args.count == 7, let port = Int(args[2]),
              let command = BridgeCommand(rawValue: "cagents daemon bridge " + args[5])
        else { throw ProbeError.usage }
        // Only software fixture keys. These inputs are deliberately file-based.
        let data = try Data(contentsOf: URL(fileURLWithPath: args[6]))
        let key = try fixtureKey(args[4], data: data)
        let connection = try await SSHConnection.open(
            SSHEndpoint(host: args[1], port: port, user: args[3],
                        fingerprint: ProcessInfo.processInfo.environment["FIXTURE_HOST_PIN"] ?? ""),
            key: key, command: command
        )
        defer { connection.close() }

        // The probe exercises an interactive stdin/stdout session even with a
        // pipe on stdin. Closing either side ends the other task.
        try await withThrowingTaskGroup(of: Void.self) { group in
            group.addTask {
                while let line = try await connection.readLine() {
                    FileHandle.standardOutput.write(Data((line + "\n").utf8))
                }
            }
            group.addTask {
                while let line = readLine() { try await connection.writeLine(line) }
                connection.close()
            }
            _ = try await group.next()
            group.cancelAll()
        }
    }

    private static func fixtureKey(_ type: String, data: Data) throws -> SSHKey {
        switch type {
        case "ed25519": return try .pairing(seed: data)
        case "p256": return try .fixtureP256(rawKey: data)
        default: throw ProbeError.usage
        }
    }

    enum ProbeError: Error { case usage }
}
