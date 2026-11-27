package dev.cagents.mobile.android

import dev.cagents.mobile.android.ssh.OneTimeKey
import org.bouncycastle.crypto.params.Ed25519PublicKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class OneTimeKeyTest {
    private fun hex(text: String) = text.chunked(2).map { it.toInt(16).toByte() }.toByteArray()
    private fun ByteArray.hex() = joinToString("") { "%02x".format(it) }

    /** RFC 8032, section 7.1, test 1: the code's seed is the key's 32-byte secret. */
    @Test
    fun derivesTheKeyTheComputerAuthorizedFromTheSeed() {
        val key = OneTimeKey(hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60"))
        assertEquals("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a", key.publicKey.hex())
        assertEquals(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
            key.sign(ByteArray(0)).hex(),
        )
        val data = "session id".toByteArray()
        val verifier = Ed25519Signer().apply { init(false, Ed25519PublicKeyParameters(key.publicKey, 0)) }
        verifier.update(data, 0, data.size)
        assertTrue(verifier.verifySignature(key.sign(data)))
    }
}
