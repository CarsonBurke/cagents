package dev.cagents.mobile.android

import com.hierynomus.sshj.userauth.certificate.Certificate
import dev.cagents.mobile.android.ssh.HostKeyStore
import dev.cagents.mobile.android.ssh.presentedKey
import net.schmizz.sshj.common.Buffer
import net.schmizz.sshj.common.KeyType
import java.security.PublicKey
import java.util.Base64
import kotlin.test.Test
import kotlin.test.assertEquals

class PresentedKeyTest {
    // Keys made with ssh-keygen, and what `ssh-keygen -lf` printed for them.
    private val ed25519 = key("AAAAC3NzaC1lZDI1NTE5AAAAIJ3c9FDS1QM1B15orJyDvW7OBEZiSvfQyd+x9QUBMNSn")
    private val ed25519Fingerprint = "SHA256:yoIFILGlsTodesSvF1knprFxI/os4Bioh8TPIuho9ao"
    private val ecdsa = key(
        "AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBIx2eYerlT1b0oVi+DyLxtbB9D5AMAteiFeibNiGZr2N3pYaNA26v/tRzwRMnPqKxjqcMNXPwWSHx5XYQ7ZC0Q4=",
    )
    private val ecdsaFingerprint = "SHA256:Uo9buvWrOtpzDo4pL0/gPyg2dagyYyqFDAVy5eV04pg"

    private fun key(base64: String): PublicKey = Buffer.PlainBuffer(Base64.getDecoder().decode(base64)).readPublicKey()

    private fun certificate(key: PublicKey): PublicKey = Certificate.getBuilder<PublicKey>().publicKey(key).build()

    @Test
    fun aPlainKeyIsFingerprintedAsSshKeygenDoes() {
        assertEquals(ed25519Fingerprint, HostKeyStore.fingerprint(presentedKey(ed25519)))
        assertEquals("ssh-ed25519", KeyType.fromKey(presentedKey(ed25519)).toString())
        assertEquals(ecdsaFingerprint, HostKeyStore.fingerprint(presentedKey(ecdsa)))
        assertEquals("ecdsa-sha2-nistp256", KeyType.fromKey(presentedKey(ecdsa)).toString())
    }

    @Test
    fun aHostCertificateIsFingerprintedAsTheKeyItCertifies() {
        for ((key, fingerprint, type) in listOf(Triple(ed25519, ed25519Fingerprint, "ssh-ed25519"), Triple(ecdsa, ecdsaFingerprint, "ecdsa-sha2-nistp256"))) {
            val presented = presentedKey(certificate(key))
            assertEquals(fingerprint, HostKeyStore.fingerprint(presented))
            assertEquals(type, KeyType.fromKey(presented).toString())
        }
    }
}
