package dev.cagents.mobile.android

import dev.cagents.mobile.android.data.ConnectionSettings
import dev.cagents.mobile.android.ui.PairingWarnings
import dev.cagents.mobile.pairing.PairingCode
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull

class PairingWarningsTest {
    private val home = "SHA256:homehomehomehomehomehomehomehomehomehome123"
    private val other = "SHA256:otherotherotherotherotherotherotherother12"
    private val paired = ConnectionSettings(
        hosts = listOf("workstation.tail1234.ts.net", "192.168.1.20"),
        port = 22,
        user = "person",
        name = "workstation",
    )

    private fun code(hostKey: String = home, hosts: List<String> = paired.hosts, port: Int = 22, user: String = "person") =
        PairingCode("workstation", user, port, hostKey, hosts, ByteArray(32), 1_800_000_300)

    private fun pins(vararg pins: Pair<String, String>): (String) -> String? = mapOf(*pins)::get

    private val pinnedHome = pins("workstation.tail1234.ts.net:22" to home, "192.168.1.20:22" to home)

    @Test
    fun theSameComputerWithNewAddressesNeedsNoWarning() {
        val warnings = PairingWarnings.of(code(hosts = listOf("Workstation.Tail1234.ts.net", "10.0.0.7")), paired, pinnedHome)
        assertFalse(warnings.any)
    }

    @Test
    fun aDifferentKeyReplacesTheComputerEvenAtAnAddressItShares() {
        // A code listing the person's real Tailscale name on another port, beside the stranger's address.
        val sneaky = code(hostKey = other, hosts = listOf("workstation.tail1234.ts.net", "203.0.113.9"), port = 2222)
        val warnings = PairingWarnings.of(sneaky, paired, pinnedHome)
        assertEquals("workstation", warnings.replaces)
        assertEquals(emptyList(), warnings.conflicts, "nothing is pinned at port 2222")
    }

    @Test
    fun aDifferentKeyWhereOneIsTrustedIsAConflict() {
        val warnings = PairingWarnings.of(code(hostKey = other), paired, pinnedHome)
        assertEquals("workstation", warnings.replaces)
        assertEquals(paired.hosts, warnings.conflicts)
    }

    @Test
    fun withNothingPinnedTheComputerIsKnownByItsLogin() {
        val manual = paired.copy(name = "", hosts = listOf("192.168.1.20"))
        assertNull(PairingWarnings.of(code(), manual, pins()).replaces)
        assertEquals("192.168.1.20", PairingWarnings.of(code(user = "someone"), manual, pins()).replaces)
        assertEquals("192.168.1.20", PairingWarnings.of(code(port = 2222), manual, pins()).replaces)
        assertEquals("192.168.1.20", PairingWarnings.of(code(hosts = listOf("10.0.0.7")), manual, pins()).replaces)
    }

    @Test
    fun aPhoneWithNoComputerYetPairsWithoutWarning() {
        assertFalse(PairingWarnings.of(code(), ConnectionSettings(), pins()).any)
    }
}
