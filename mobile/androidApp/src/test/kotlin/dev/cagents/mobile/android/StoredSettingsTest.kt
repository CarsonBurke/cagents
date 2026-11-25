package dev.cagents.mobile.android

import dev.cagents.mobile.android.data.ConnectionSettings
import kotlin.test.Test
import kotlin.test.assertEquals

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
}
