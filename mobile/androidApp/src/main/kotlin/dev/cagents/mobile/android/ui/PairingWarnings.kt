package dev.cagents.mobile.android.ui

import dev.cagents.mobile.android.data.ConnectionSettings
import dev.cagents.mobile.pairing.PairingCode

/**
 * What to tell the person before pairing with a code: whether it [replaces]
 * the computer this phone uses now (its name), and which of the code's
 * addresses this phone [conflicts] with, trusting another key there.
 */
data class PairingWarnings(val replaces: String?, val conflicts: List<String>) {
    val any: Boolean get() = replaces != null || conflicts.isNotEmpty()

    companion object {
        /**
         * Compares by identity: the computer this phone trusts is the one
         * whose host key it pinned, whatever addresses or port a code
         * lists. Only a computer never connected to, with nothing pinned,
         * is recognized by its port, user and addresses.
         */
        fun of(code: PairingCode, current: ConnectionSettings, pinned: (hostKeyId: String) -> String?): PairingWarnings {
            val atCodePort = current.copy(port = code.port)
            val conflicts = code.hosts.filter { host ->
                pinned(atCodePort.hostKeyId(host)).let { it != null && it != code.hostKey }
            }
            val replaces = if (!current.isComplete) {
                null
            } else {
                val pins = current.hostKeyIds.mapNotNull(pinned)
                val same = if (pins.isNotEmpty()) {
                    code.hostKey in pins
                } else {
                    val hosts = current.hosts.map { it.trim().lowercase() }.toSet()
                    current.port == code.port &&
                        current.user == code.user &&
                        code.hosts.any { it.trim().lowercase() in hosts }
                }
                current.label.takeUnless { same }
            }
            return PairingWarnings(replaces, conflicts)
        }
    }
}
