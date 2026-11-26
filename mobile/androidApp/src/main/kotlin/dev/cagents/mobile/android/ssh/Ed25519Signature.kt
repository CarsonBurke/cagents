package dev.cagents.mobile.android.ssh

import net.schmizz.sshj.common.Buffer
import net.schmizz.sshj.common.KeyType
import net.schmizz.sshj.common.SSHRuntimeException
import net.schmizz.sshj.signature.Signature
import org.bouncycastle.crypto.params.Ed25519PublicKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer
import java.io.ByteArrayOutputStream
import java.security.PrivateKey
import java.security.PublicKey

/**
 * `ssh-ed25519` for sshj: signs with a [SigningKey], such as the [PhoneKey]
 * wherever it lives, and verifies the computer's host key signatures with
 * BouncyCastle.
 */
internal class Ed25519Signature(private val key: SigningKey) : Signature {
    class Factory(private val key: SigningKey) : net.schmizz.sshj.common.Factory.Named<Signature> {
        override fun getName() = KeyType.ED25519.toString()
        override fun create(): Signature = Ed25519Signature(key)
    }

    private companion object {
        val ED25519_ALGORITHMS = setOf("Ed25519", "EdDSA")
    }

    private val data = ByteArrayOutputStream()
    private var verifier: Ed25519Signer? = null

    override fun getSignatureName() = KeyType.ED25519.toString()

    override fun initSign(prvkey: PrivateKey) {
        verifier = null
        data.reset()
    }

    override fun initVerify(pubkey: PublicKey) {
        // Only an Ed25519 key in its X.509 encoding: the prefix names the curve,
        // so the trailing 32 bytes are never read out of some other kind of key.
        val encoded: ByteArray? = pubkey.encoded
        val prefix = Ed25519PublicKey.X509_PREFIX
        if (
            pubkey.algorithm !in ED25519_ALGORITHMS ||
            encoded == null ||
            encoded.size != prefix.size + Ed25519PublicKeyParameters.KEY_SIZE ||
            !encoded.copyOfRange(0, prefix.size).contentEquals(prefix)
        ) {
            throw SSHRuntimeException("Expected an Ed25519 host key, got ${pubkey.algorithm}")
        }
        val raw = Ed25519PublicKeyParameters(encoded, prefix.size)
        verifier = Ed25519Signer().apply { init(false, raw) }
        data.reset()
    }

    override fun update(H: ByteArray) = update(H, 0, H.size)

    override fun update(H: ByteArray, off: Int, len: Int) {
        val verifying = verifier
        if (verifying != null) verifying.update(H, off, len) else data.write(H, off, len)
    }

    override fun sign(): ByteArray = key.sign(data.toByteArray()).also { data.reset() }

    override fun encode(signature: ByteArray) = signature

    override fun verify(sig: ByteArray): Boolean {
        val verifying = verifier ?: throw SSHRuntimeException("Not initialized to verify")
        val buffer = Buffer.PlainBuffer(sig)
        val algorithm = buffer.readString()
        if (algorithm != signatureName) throw SSHRuntimeException("Expected '$signatureName' but got '$algorithm'")
        return verifying.verifySignature(buffer.readBytes())
    }
}
