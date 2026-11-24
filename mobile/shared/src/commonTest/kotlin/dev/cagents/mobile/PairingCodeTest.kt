package dev.cagents.mobile

import dev.cagents.mobile.pairing.PairingCode
import dev.cagents.mobile.pairing.PairingCode.Parsed
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

class PairingCodeTest {
    private val now = 1_800_000_000L

    // As the computer writes it, with the url crate's form encoding: `+` for a
    // space and %XX for the fingerprint's `:`, `+` and `/`.
    private val fingerprint = "SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s"
    private val seed = ByteArray(32) { it.toByte() }
    private val seedText = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"

    private fun code(
        v: String = "v=1",
        name: String = "name=work+station",
        hostKey: String = "hostkey=SHA256%3AuNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s",
        hosts: String = "host=workstation.tail1234.ts.net&host=100.101.102.103&host=192.168.1.20",
        key: String = "key=$seedText",
        expires: String = "expires=${now + 300}",
        extra: String = "",
    ) = listOf(v, name, "user=person", "port=2222", hostKey, hosts, key, expires, extra)
        .filter { it.isNotEmpty() }
        .joinToString("&", prefix = "cagents-mobile://pair?")

    private fun parsed(text: String, at: Long = now) = PairingCode.parse(text, at)

    private fun valid(text: String) = assertIs<Parsed.Code>(parsed(text)).code

    private fun refused(text: String, at: Long = now): String = assertIs<Parsed.Invalid>(parsed(text, at)).message

    @Test
    fun readsEveryFieldOfTheComputersCode() {
        val code = valid(code())
        assertEquals("work station", code.name)
        assertEquals("person", code.user)
        assertEquals(2222, code.port)
        assertEquals(fingerprint, code.hostKey)
        assertEquals(listOf("workstation.tail1234.ts.net", "100.101.102.103", "192.168.1.20"), code.hosts)
        assertContentEquals(seed, code.seed)
        assertEquals(now + 300, code.expires)
    }

    @Test
    fun keepsTheHostsInTheirOrderWithoutRepeats() {
        val code = valid(code(hosts = "host=b&host=a&host=b&host=&host=c"))
        assertEquals(listOf("b", "a", "c"), code.hosts)
    }

    @Test
    fun decodesPercentEscapesAndUtf8() {
        val code = valid(code(name = "name=K%C3%BCche%20PC%2B1"))
        assertEquals("Küche PC+1", code.name)
        // An unescaped fingerprint reads the same, as a pasted or hand-made code may have it.
        assertEquals(fingerprint, valid(code(hostKey = "hostkey=$fingerprint")).hostKey)
        assertTrue(refused(code(name = "name=bad%ZZ")).isNotEmpty())
        assertTrue(refused(code(name = "name=bad%-1")).isNotEmpty())
        assertTrue(refused(code(name = "name=truncated%C3")).isNotEmpty())
    }

    @Test
    fun ignoresParametersItDoesNotKnowAndCaseInTheScheme() {
        val code = valid(code(extra = "later=1").replace("cagents-mobile://", "CAGENTS-MOBILE://"))
        assertEquals("person", code.user)
    }

    @Test
    fun withoutANameShowsTheFirstHost() {
        assertEquals("workstation.tail1234.ts.net", valid(code(name = "")).name)
    }

    @Test
    fun refusesAVersionItDoesNotKnow() {
        assertTrue("newer version" in refused(code(v = "v=2")))
        assertTrue("newer version" in refused(code(v = "v=one")))
        assertIs<Parsed.Invalid>(parsed(code(v = "")))
    }

    @Test
    fun refusesAnExpiredCodeAllowingSomeClockSkew() {
        val expires = now + 300
        assertIs<Parsed.Code>(parsed(code(), at = expires))
        assertIs<Parsed.Code>(parsed(code(), at = expires + PairingCode.CLOCK_SKEW_SECONDS))
        assertTrue("expired" in refused(code(), at = expires + PairingCode.CLOCK_SKEW_SECONDS + 1))
        assertIs<Parsed.Invalid>(parsed(code(expires = "expires=soon")))
        assertIs<Parsed.Invalid>(parsed(code(expires = "")))
    }

    @Test
    fun refusesACodeThatCannotPinTheComputer() {
        assertTrue("recognize the computer" in refused(code(hostKey = "")))
        assertTrue("recognize the computer" in refused(code(hostKey = "hostkey=MD5:ab:cd")))
        assertTrue("recognize the computer" in refused(code(hostKey = "hostkey=SHA256%3Ashort")))
    }

    @Test
    fun refusesMissingOrMalformedFields() {
        for (broken in listOf(
            code(hosts = ""),
            code(hosts = "host=a&host=b c"),
            code(hosts = "host=user@a"),
            code(key = ""),
            code(key = "key=AAEC"),
            code(key = "key=$seedText=="),
            code(key = "key=not*base64"),
            code().replace("port=2222", "port=0"),
            code().replace("port=2222", "port=70000"),
            code().replace("&port=2222", ""),
            code().replace("user=person", "user="),
            code().replace("user=person", "user=a+b"),
            // A field given twice could be read either way.
            code(extra = "user=other"),
            code(extra = "hostkey=SHA256%3AAAAAztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s"),
            code().replace("://pair?", "://other?"),
            "cagents-mobile://pair",
        )) {
            assertIs<Parsed.Invalid>(parsed(broken), broken)
        }
    }

    @Test
    fun anythingElseIsNotACode() {
        for (other in listOf("https://example.com/?v=1", "WIFI:S:home;T:WPA;P:secret;;", "", "cagents-mobilex://pair?v=1")) {
            assertEquals(Parsed.NotACode, parsed(other), other)
        }
    }

    @Test
    fun neverPrintsTheOneTimeKey() {
        assertTrue(seedText !in valid(code()).toString())
    }
}
