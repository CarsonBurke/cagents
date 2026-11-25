package dev.cagents.mobile.android

import dev.cagents.mobile.android.ssh.describe
import net.schmizz.sshj.transport.TransportException
import java.io.IOException
import java.net.ConnectException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import kotlin.test.Test
import kotlin.test.assertEquals

class DescribeFailureTest {
    @Test
    fun aRefusalIsKnownUnderTheSystemsOwnError() {
        // As Android reports it: the errno beneath the ConnectException.
        val refused = ConnectException("failed to connect to /10.0.2.2 (port 2244): isConnected failed: ECONNREFUSED")
            .initCause(IOException("isConnected failed: ECONNREFUSED"))
        assertEquals(
            "Couldn't reach 10.0.2.2:2244. Is the computer awake and on the same network?",
            describe(refused, "10.0.2.2", 2244),
        )
    }

    @Test
    fun namesTheKindWhereverItIsInTheChain() {
        assertEquals("Couldn't find nowhere.local. Check the address.", describe(TransportException(UnknownHostException("nowhere.local")), "nowhere.local", 22))
        assertEquals("h:22 didn't answer in time.", describe(SocketTimeoutException(), "h", 22))
        assertEquals("Something else", describe(IOException("Something else"), "h", 22))
    }
}
