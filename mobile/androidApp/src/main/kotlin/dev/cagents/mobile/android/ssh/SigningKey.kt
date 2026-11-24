package dev.cagents.mobile.android.ssh

import android.util.Base64
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer

/** An Ed25519 key that SSH can authenticate with. */
interface SigningKey {
    /** The raw 32-byte public key. */
    val publicKey: ByteArray

    fun sign(data: ByteArray): ByteArray

    /** The public key in OpenSSH form, `ssh-ed25519 AAAA…`, without a comment. */
    val openSshKey: String
        get() = "ssh-ed25519 ${Base64.encodeToString(PhoneKey.publicBlob(publicKey), Base64.NO_WRAP)}"

    /** OpenSSH's SHA-256 fingerprint of the public key, as `ssh-keygen -lf` prints it. */
    val fingerprint: String
        get() = HostKeyStore.fingerprint(PhoneKey.publicBlob(publicKey))
}

/** The pairing code's one-time key, from its 32-byte seed. It is never stored. */
class OneTimeKey(seed: ByteArray) : SigningKey {
    private val private = Ed25519PrivateKeyParameters(seed, 0)
    override val publicKey: ByteArray = private.generatePublicKey().encoded

    override fun sign(data: ByteArray): ByteArray = Ed25519Signer().run {
        init(true, private)
        update(data, 0, data.size)
        generateSignature()
    }
}
