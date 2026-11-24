package dev.cagents.mobile.pairing

import kotlin.io.encoding.Base64

/**
 * A pairing code, as the computer's phone popover shows it: a
 * `cagents-mobile://pair?…` URI. See the "Pairing" section of
 * docs/mobile-api.md.
 */
class PairingCode(
    /** The computer's host name, to show. */
    val name: String,
    val user: String,
    val port: Int,
    /** The computer's Ed25519 host key fingerprint, `SHA256:…` as `ssh-keygen -lf` prints it. */
    val hostKey: String,
    /** The addresses to try, in order: Tailscale first, then the local network. */
    val hosts: List<String>,
    /** The one-time Ed25519 key's 32-byte seed. */
    val seed: ByteArray,
    /** When the code stops working, in Unix seconds. */
    val expires: Long,
) {
    override fun toString() = "PairingCode(name=$name, user=$user, port=$port, hosts=$hosts, expires=$expires)"

    companion object {
        const val SCHEME = "cagents-mobile"
        const val VERSION = 1

        /**
         * How far past [expires] a code is still tried, for a phone whose
         * clock runs a little ahead. The computer judges by its own clock.
         */
        const val CLOCK_SKEW_SECONDS = 60L

        private val SINGLE = listOf("v", "name", "user", "port", "hostkey", "key", "expires")
        private val FINGERPRINT = Regex("SHA256:[A-Za-z0-9+/]{43}")
        private val SEED = Base64.UrlSafe.withPadding(Base64.PaddingOption.ABSENT)

        /** Reads [text] as a pairing code, judging its expiry at [nowSeconds]. */
        fun parse(text: String, nowSeconds: Long): Parsed {
            val uri = text.trim()
            val schemeEnd = uri.indexOf("://")
            if (schemeEnd < 0 || !uri.substring(0, schemeEnd).equals(SCHEME, ignoreCase = true)) {
                return Parsed.NotACode
            }
            val rest = uri.substring(schemeEnd + 3).substringBefore('#')
            val target = rest.substringBefore('?').trimEnd('/')
            if (!target.equals("pair", ignoreCase = true) || '?' !in rest) return invalid()
            val params = mutableMapOf<String, MutableList<String>>()
            for (pair in rest.substringAfter('?').split('&')) {
                if (pair.isEmpty()) continue
                val name = formDecode(pair.substringBefore('=')) ?: return invalid()
                val value = formDecode(pair.substringAfter('=', "")) ?: return invalid()
                params.getOrPut(name) { mutableListOf() } += value
            }
            // A parameter given twice can't be read one way, so the code isn't trusted.
            if (SINGLE.any { (params[it]?.size ?: 0) > 1 }) return invalid()
            fun one(name: String) = params[name]?.single()

            val version = one("v") ?: return invalid()
            if (version.toIntOrNull() != VERSION) {
                return Parsed.Invalid("This pairing code needs a newer version of the app. Update it, then scan again.")
            }
            val expires = one("expires")?.toLongOrNull() ?: return invalid()
            if (nowSeconds > expires + CLOCK_SKEW_SECONDS) {
                return Parsed.Invalid("This pairing code has expired. Show a new one on the computer.")
            }
            val hostKey = one("hostkey")?.takeIf(FINGERPRINT::matches) ?: return Parsed.Invalid(
                "This pairing code doesn't say how to recognize the computer. Update cagents there and show a new code.",
            )
            val user = one("user")?.takeIf(::isPlainWord) ?: return invalid()
            val port = one("port")?.toIntOrNull()?.takeIf { it in 1..65535 } ?: return invalid()
            val hosts = params["host"].orEmpty().map { it.trim() }.filter { it.isNotEmpty() }.distinct()
            if (hosts.isEmpty() || !hosts.all(::isPlainWord)) return invalid()
            val seed = one("key")?.let { runCatching { SEED.decode(it) }.getOrNull() }
                ?.takeIf { it.size == 32 } ?: return invalid()
            val name = one("name")?.trim()?.takeIf { it.isNotEmpty() } ?: hosts.first()
            return Parsed.Code(PairingCode(name, user, port, hostKey, hosts, seed, expires))
        }

        private fun invalid() = Parsed.Invalid("This pairing code isn't complete. Show a new one on the computer and scan it again.")

        /** A host or user name: something to connect to, not a URI or a list. */
        private fun isPlainWord(text: String) =
            text.isNotEmpty() && text.length <= 255 && text.none { it.isWhitespace() || it.isISOControl() || it in "/@?#" }

        /**
         * `application/x-www-form-urlencoded` decoding, as the computer
         * encodes the query: `+` is a space and `%XX` a UTF-8 byte. Null for
         * a malformed escape or invalid UTF-8.
         */
        internal fun formDecode(text: String): String? {
            if ('%' !in text && '+' !in text) return text
            val bytes = ArrayList<Byte>(text.length)
            var i = 0
            while (i < text.length) {
                when (text[i]) {
                    '+' -> {
                        bytes += ' '.code.toByte()
                        i++
                    }
                    '%' -> {
                        val high = text.getOrNull(i + 1)?.digitToIntOrNull(16) ?: return null
                        val low = text.getOrNull(i + 2)?.digitToIntOrNull(16) ?: return null
                        bytes += (high * 16 + low).toByte()
                        i += 3
                    }
                    else -> {
                        // A run of literal characters, encoded together so surrogate pairs stay whole.
                        var end = i
                        while (end < text.length && text[end] != '+' && text[end] != '%') end++
                        text.substring(i, end).encodeToByteArray().forEach { bytes += it }
                        i = end
                    }
                }
            }
            return runCatching { bytes.toByteArray().decodeToString(throwOnInvalidSequence = true) }.getOrNull()
        }
    }

    sealed interface Parsed {
        data class Code(val code: PairingCode) : Parsed

        /** A pairing code that can't be used; [message] is written for the person. */
        data class Invalid(val message: String) : Parsed

        /** Not a cagents pairing code at all, such as some other QR code. */
        data object NotACode : Parsed
    }
}
