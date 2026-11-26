package dev.cagents.mobile

import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.client.isTailscale
import dev.cagents.mobile.client.mostTelling
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertSame
import kotlin.test.assertTrue

class MostTellingTest {
    private val tailscale = "workstation.tail1234.ts.net"
    private val lan = "192.168.1.20"
    private val hosts = listOf(tailscale, "100.101.102.103", lan)
    private val unreachable get() = ConnectFailure.Network("Couldn't reach it.")
    private fun changed(host: String) = ConnectFailure.HostKeyChanged("SHA256:new", "SHA256:old", host)

    @Test
    fun aStrangerAtALocalAddressOfAPairedComputerIsOnlyUnreachable() {
        // Away from home: Tailscale is off and someone else's machine has the home LAN address.
        val stranger = changed(lan)
        val reported = mostTelling(listOf(unreachable, unreachable, stranger), "workstation", hosts, paired = true)
        val network = assertIs<ConnectFailure.Network>(reported)
        assertTrue(network.retry, "it keeps trying, and connects once the computer is in reach")
        assertTrue(lan in network.message!!)
        assertTrue("Tailscale" in network.message!!)
        assertSame(stranger, network.cause)
        // A computer with no Tailscale address isn't one to suggest it for.
        val local = listOf("192.168.1.21", lan)
        val elsewhere = mostTelling(listOf(unreachable, stranger), "workstation", local, paired = true)
        assertFalse("Tailscale" in elsewhere.message!!)
    }

    @Test
    fun aComputerThatRefusesThePortSaysSoRatherThanUnreachable() {
        val refused = ConnectFailure.Network("100.101.102.103 refused the connection on port 2244.", answered = true)
        val reported = mostTelling(listOf(unreachable, refused, unreachable), "workstation", hosts, paired = true)
        assertSame(refused, reported)
        // With nothing refusing, it stays the summary of every address.
        val none = mostTelling(listOf(unreachable, unreachable), "workstation", hosts, paired = true)
        assertTrue("any of its 3 addresses" in none.message!!)
    }

    @Test
    fun aPairedComputerWithAWrongKeyEverywhereRaisesTheAlarm() {
        val failures = hosts.map(::changed)
        assertSame(failures.first(), mostTelling(failures, "workstation", hosts, paired = true))
    }

    @Test
    fun aWrongKeyAtATailscaleAddressRaisesTheAlarm() {
        for (host in listOf(tailscale, "100.101.102.103")) {
            val wrong = changed(host)
            val failures = hosts.map { if (it == host) wrong else unreachable }
            assertSame(wrong, mostTelling(failures, "workstation", hosts, paired = true), host)
        }
    }

    @Test
    fun anAddressTypedByHandRaisesTheAlarmWhereverItIs() {
        val wrong = changed(lan)
        assertSame(wrong, mostTelling(listOf(unreachable, wrong), lan, listOf("10.0.0.9", lan), paired = false))
    }

    @Test
    fun aKeyToTrustComesBeforeARefusalBeforeNothingAnswering() {
        val unknown = ConnectFailure.UnknownHostKey("SHA256:new", "ssh-ed25519", lan)
        val refused = ConnectFailure.Refused("No bridge here.")
        assertSame(unknown, mostTelling(listOf(refused, unreachable, unknown), "w", hosts, paired = false))
        assertSame(refused, mostTelling(listOf(unreachable, refused, unreachable), "w", hosts, paired = false))
    }

    @Test
    fun oneAddressReportsItsOwnFailureAndSeveralSaySoTogether() {
        val only = unreachable
        assertSame(only, mostTelling(listOf(only), "w", listOf(lan), paired = true))
        val all = mostTelling(listOf(unreachable, unreachable, unreachable), "workstation", hosts, paired = true)
        assertEquals(
            "Couldn't reach workstation at any of its 3 addresses. Is it awake, and is this phone on its network or on Tailscale?",
            all.message,
        )
    }

    @Test
    fun recognizesTailscaleAddresses() {
        for (host in listOf(tailscale, "WORKSTATION.TAIL1234.TS.NET", "100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::1", "[fd7a:115c:a1e0:ab12::1]")) {
            assertTrue(isTailscale(host), host)
        }
        for (host in listOf(lan, "100.63.0.1", "100.128.0.1", "10.0.0.2", "workstation.local", "fd00::1", "100.64.0")) {
            assertFalse(isTailscale(host), host)
        }
    }
}
