# iOS connection core

The shared Kotlin core builds for iPhone and Apple silicon simulators. The
recordings in `crates/mobile-api/fixtures` feed the same compatibility tests
on JVM and iOS.

The Swift package in `mobile/` connects the shared `BridgeTransport` to
SwiftNIO SSH. To pair, open `NativeTransport.pairing(code:host:)` with the
one-time Ed25519 key from the code and send the public key from
`PhoneKeyStore` through the shared `PairingExchange`. The persisted phone key
is P-256. Only the device-bound representation goes into the Keychain, and
SwiftNIO asks the Secure Enclave to sign. After approval,
`NativeTransport.mobile(endpoint:phoneKey:)` runs the mobile bridge with the
phone key and the computer's pinned host fingerprint.

## SSH choice

Both candidates ran the API v1 welcome and ping exchange over loopback
OpenSSH on fw16. The probes used disposable software keys and rejected a
wrong host pin and an unlisted phone key. The table covers ten sequential
connections per key after a warmup.

| Candidate | Ed25519 successes, median | P-256 successes, median | Stripped Linux payload |
| --- | ---: | ---: | ---: |
| SwiftNIO SSH 0.15.0 | 10/10, 211 ms | 10/10, 212 ms | 7.07 MB including the linked SSH and crypto code |
| libssh2 1.11.1 with OpenSSL | 10/10, 243 ms | 10/10, 229 ms | 6.38 MB including libssh2, OpenSSL and zlib shared libraries |

These Linux measurements exclude each platform's system and language
runtimes. An iOS link and size measurement requires Xcode because SwiftNIO's
crypto uses CryptoKit on Apple platforms.

SwiftNIO meets the observed reliability and compatibility requirements and
accepts a Secure Enclave signing key directly. libssh2 can call an external
signer, but the app would need to encode the P-256 signature for SSH and
maintain a C crypto backend for iOS. [SwiftNIO key implementation](https://github.com/apple/swift-nio-ssh/blob/0.15.0/Sources/NIOSSH/Keys%20And%20Signatures/NIOSSHPrivateKey.swift)
[libssh2 signer API](https://libssh2.org/libssh2_userauth_publickey.html)
[Apple Keychain example](https://developer.apple.com/documentation/cryptokit/storing-cryptokit-keys-in-the-keychain)

## Verify on fw16

From a fw16 checkout containing the P-256 pairing change, build the Swift
probe and point it at a matched `cagents` and `cagentsd` build. Set
`FIXTURE_BIN_DIR` to their directory.

```sh
swift build --package-path mobile/ios --jobs 2 -c release
python3 -I -B mobile/ios/test-fixture.py \
  mobile/ios/.build/release/cagents-ssh-probe "$FIXTURE_BIN_DIR" --shared --pair
```

The fixture uses a temporary SSH server, keys and daemon. `--shared` drives
hello, ping and a session watch through the Kotlin client and Swift SSH.
`--pair` exercises the one-time code and reconnects with the approved P-256
key. Use a computer build with P-256 pairing for `--pair`.

## Verify on macOS

On an authorized Mac with full Xcode and an installed iOS simulator, run
from `mobile/`. Use the iPhone SDK for the device build. Both builds can
run without signing.

```sh
CAGENTS_GRADLE_LOCAL=1 ./gradlew :shared:iosSimulatorArm64Test \
  :shared:assembleCagentsMobileSharedDebugXCFramework
xcodebuild -scheme CagentsNativeTransport -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath build/ios-simulator CODE_SIGNING_ALLOWED=NO build
xcodebuild -scheme CagentsNativeTransport -destination 'generic/platform=iOS' \
  -derivedDataPath build/ios-device CODE_SIGNING_ALLOWED=NO build
```

The MacBook available for this work had only Command Line Tools. The shared
device and simulator Kotlin sources and simulator test sources compiled on
fw16. Running those tests, linking the Swift/Kotlin adapter, measuring the
iOS package and exercising hardware-backed signing remain to be verified.
