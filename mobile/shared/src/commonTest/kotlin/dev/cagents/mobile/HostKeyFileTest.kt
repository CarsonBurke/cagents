package dev.cagents.mobile

import dev.cagents.mobile.client.hostKeyFile
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

class HostKeyFileTest {
    @Test
    fun namesTheFileOfTheNegotiatedKeyType() {
        assertEquals("/etc/ssh/ssh_host_ed25519_key.pub", hostKeyFile("ssh-ed25519"))
        assertEquals("/etc/ssh/ssh_host_ecdsa_key.pub", hostKeyFile("ecdsa-sha2-nistp256"))
        assertEquals("/etc/ssh/ssh_host_ecdsa_key.pub", hostKeyFile("ecdsa-sha2-nistp521"))
        assertEquals("/etc/ssh/ssh_host_rsa_key.pub", hostKeyFile("ssh-rsa"))
        assertEquals("/etc/ssh/ssh_host_rsa_key.pub", hostKeyFile("rsa-sha2-512"))
    }

    @Test
    fun aCertificateNamesTheFileOfTheKeyItCertifies() {
        assertEquals("/etc/ssh/ssh_host_ed25519_key.pub", hostKeyFile("ssh-ed25519-cert-v01@openssh.com"))
        assertEquals("/etc/ssh/ssh_host_rsa_key.pub", hostKeyFile("ssh-rsa-cert-v01@openssh.com"))
    }

    @Test
    fun anUnknownTypeNamesNoFile() {
        assertNull(hostKeyFile("sk-ssh-ed25519@openssh.com"))
        assertNull(hostKeyFile(""))
    }
}
