package dev.cagents.mobile

import dev.cagents.mobile.pairing.PairingCode
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertIs

/**
 * Reads the pairing code the computer's own code writes for known inputs
 * (`the_code_is_the_one_recorded_for_the_app` in `src/mobile/pairing_tests.rs`).
 */
class RecordedPairingCodeTest {
    private val recorded = pairingCodeRecording.single().trim()

    @Test
    fun readsTheCodeTheComputerWrites() {
        val code = assertIs<PairingCode.Parsed.Code>(PairingCode.parse(recorded, 1_800_000_000)).code
        assertEquals("Büro station", code.name)
        assertEquals("person", code.user)
        assertEquals(2222, code.port)
        assertEquals("SHA256:uNiVztksCs+hcc0u9e8BujQXVUpKZIDTMczCvj3t/2s", code.hostKey)
        assertEquals(listOf("workstation.tail1234.ts.net", "100.101.102.103", "192.168.1.20"), code.hosts)
        assertContentEquals(ByteArray(32) { it.toByte() }, code.seed)
        assertEquals(1_800_000_300, code.expires)
    }
}
