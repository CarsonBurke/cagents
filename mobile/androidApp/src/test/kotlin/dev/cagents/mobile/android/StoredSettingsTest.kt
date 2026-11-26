package dev.cagents.mobile.android

import dev.cagents.mobile.android.data.ConnectionSettings
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class StoredSettingsTest {
    @Test
    fun aComputerSavedBeforeThereCouldBeSeveralAddressesKeepsItsOne() {
        assertEquals(listOf("workstation.local"), ConnectionSettings.storedHosts(hosts = null, host = " workstation.local "))
        assertEquals(emptyList(), ConnectionSettings.storedHosts(hosts = null, host = null))
    }

    @Test
    fun severalAddressesComeBackInOrder() {
        assertEquals(
            listOf("workstation.tail1234.ts.net", "192.168.1.20"),
            ConnectionSettings.storedHosts(hosts = "workstation.tail1234.ts.net\n\n192.168.1.20\n", host = "workstation.tail1234.ts.net"),
        )
    }

    @Test
    fun pinsMadeForTheOldSingleAddressStillApply() {
        // Pins were keyed host:port, lowercased, before there could be several addresses.
        val settings = ConnectionSettings(hosts = ConnectionSettings.storedHosts(null, "Workstation.local"), port = 2222, user = "person")
        assertEquals(listOf("workstation.local:2222"), settings.hostKeyIds)
    }

    @Test
    fun anotherAddressPortUserOrCommandIsAnotherComputer() {
        val computer = ConnectionSettings(hosts = listOf("workstation.tail1234.ts.net", "192.168.1.20"), port = 22, user = "person")
        assertTrue(computer.sameComputer(computer.copy(hosts = listOf("192.168.1.20", "Workstation.tail1234.ts.net"))))
        assertTrue(computer.sameComputer(computer.copy(name = "workstation")))
        assertFalse(computer.sameComputer(computer.copy(hosts = listOf("192.168.1.30"))))
        assertFalse(computer.sameComputer(computer.copy(hosts = computer.hosts + "192.168.1.30")))
        assertFalse(computer.sameComputer(computer.copy(port = 2222)))
        assertFalse(computer.sameComputer(computer.copy(user = "other")))
        assertFalse(computer.sameComputer(computer.copy(command = "/opt/cagents daemon bridge --mobile")))
        assertFalse(ConnectionSettings().sameComputer(computer))
    }
}
