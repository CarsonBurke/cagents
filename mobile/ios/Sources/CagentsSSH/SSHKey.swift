import Crypto
import Foundation
import NIOSSH

package struct SSHKey: Sendable {
    let key: NIOSSHPrivateKey

    package static func pairing(seed: Data) throws -> Self {
        SSHKey(key: NIOSSHPrivateKey(ed25519Key: try .init(rawRepresentation: seed)))
    }

    package static func fixtureP256(rawKey: Data) throws -> Self {
        SSHKey(key: NIOSSHPrivateKey(p256Key: try .init(rawRepresentation: rawKey)))
    }

    package var publicKey: String { String(openSSHPublicKey: key.publicKey) }
}

#if os(iOS)
import Security

public struct PhoneKey: Sendable {
    let ssh: SSHKey

    public var publicKey: String { ssh.publicKey }
}

public struct PhoneKeyStore {
    private let service: String

    public init(service: String) { self.service = service }

    /// The keychain holds CryptoKit's device-bound encrypted representation.
    /// Authentication signs through the Secure Enclave in NIOSSH.
    public func loadOrCreate() throws -> PhoneKey {
        guard SecureEnclave.isAvailable else { throw KeychainError(status: errSecNotAvailable) }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: "mobile-ssh-p256",
        ]
        if let key = try load(query) { return key }

        guard let access = SecAccessControlCreateWithFlags(
            nil, kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly, .privateKeyUsage, nil
        ) else { throw KeychainError(status: errSecParam) }
        let enclaveKey = try SecureEnclave.P256.Signing.PrivateKey(accessControl: access)
        var item = query
        item[kSecValueData as String] = enclaveKey.dataRepresentation
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(item as CFDictionary, nil)
        if status == errSecDuplicateItem, let key = try load(query) { return key }
        guard status == errSecSuccess else { throw KeychainError(status: status) }
        return PhoneKey(ssh: SSHKey(key: .init(secureEnclaveP256Key: enclaveKey)))
    }

    private func load(_ query: [String: Any]) throws -> PhoneKey? {
        var lookup = query
        lookup[kSecReturnData as String] = true
        lookup[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(lookup as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else {
            throw KeychainError(status: status == errSecSuccess ? errSecDecode : status)
        }
        let key = try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: data)
        return PhoneKey(ssh: SSHKey(key: .init(secureEnclaveP256Key: key)))
    }

    public struct KeychainError: Error {
        public let status: OSStatus
    }
}
#endif
