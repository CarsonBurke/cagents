package dev.cagents.mobile.android

import dev.cagents.mobile.android.ssh.describeConnectFailure
import dev.cagents.mobile.android.ssh.isRefusal
import net.schmizz.sshj.transport.TransportException
import java.io.IOException
import java.net.ConnectException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class DescribeFailureTest {
    @Test
    fun aRefusalIsKnownUnderTheSystemsOwnError() {
        // As Android reports it: the errno beneath the ConnectException.
        val refused = ConnectException("failed to connect to /10.0.2.2 (port 2244): isConnected failed: ECONNREFUSED")
            .initCause(IOException("isConnected failed: ECONNREFUSED"))
        assertEquals(
            "10.0.2.2 refused the connection on port 2244. Is its SSH server running on that port? " +
                "Check the port in Settings, or pair again.",
            describeConnectFailure(refused, "10.0.2.2", 2244),
        )
        assertTrue(isRefusal(refused))
    }

    @Test
    fun anAddressThatIsntThereIsNotARefusal() {
        val unreachable = ConnectException("failed to connect to /100.64.0.1 (port 22): connect failed: EHOSTUNREACH (No route to host)")
        assertEquals(
            "Couldn't reach 100.64.0.1:22. Is the computer awake and on the same network?",
            describeConnectFailure(unreachable, "100.64.0.1", 22),
        )
        assertFalse(isRefusal(unreachable))
    }

    @Test
    fun namesTheKindWhereverItIsInTheChain() {
        assertEquals("Couldn't find nowhere.local. Check the address.", describeConnectFailure(TransportException(UnknownHostException("nowhere.local")), "nowhere.local", 22))
        assertEquals("h:22 didn't answer in time.", describeConnectFailure(SocketTimeoutException(), "h", 22))
        assertEquals("Something else", describeConnectFailure(IOException("Something else"), "h", 22))
    }
}
