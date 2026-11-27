package dev.cagents.mobile.android

import dev.cagents.mobile.android.ssh.CodeHostKey
import dev.cagents.mobile.android.ssh.HostKeyStore
import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.pairing.PairingCode
import kotlin.test.Test
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue

class CodeHostKeyTest {
    private val fingerprint = "SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s"
    private val check = CodeHostKey(
        PairingCode("workstation", "person", 22, fingerprint, listOf("192.168.1.20"), ByteArray(32), 1_800_000_300),
    )

    @Test
    fun trustsOnlyTheEd25519KeyTheCodeNames() {
        assertTrue(check.ed25519Only)
        assertTrue(check.preferred("192.168.1.20") == listOf(HostKeyStore.ED25519))
        assertNull(check.check("192.168.1.20", fingerprint, HostKeyStore.ED25519))
    }

    @Test
    fun refusesAnyOtherKeyWithoutOfferingToTrustIt() {
        val wrong = check.check("192.168.1.20", "SHA256:otherotherotherotherotherotherotherother12", HostKeyStore.ED25519)
        assertIs<ConnectFailure.Refused>(wrong)
        // The same fingerprint text on another type of key isn't the same key.
        assertIs<ConnectFailure.Refused>(check.check("192.168.1.20", fingerprint, "ssh-rsa"))
    }
}
