package dev.cagents.mobile.android.ui

import dev.cagents.mobile.android.data.ConnectionSettings
import dev.cagents.mobile.android.ssh.HostKeyStore
import dev.cagents.mobile.android.ssh.PinnedKey
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
         * lists. A code names the computer's Ed25519 key, so only an
         * Ed25519 pin can be compared with it: one of another type, trusted
         * on first use, may be the same computer's other key. A computer
         * with no such pin is recognized by its port, user and addresses.
         */
        fun of(code: PairingCode, current: ConnectionSettings, pinned: (hostKeyId: String) -> PinnedKey?): PairingWarnings {
            fun ed25519(id: String) = pinned(id)?.takeIf { it.type == null || it.type == HostKeyStore.ED25519 }?.fingerprint
            val atCodePort = current.copy(port = code.port)
            val conflicts = code.hosts.filter { host ->
                ed25519(atCodePort.hostKeyId(host)).let { it != null && it != code.hostKey }
            }
            val replaces = if (!current.isComplete) {
                null
            } else {
                val pins = current.hostKeyIds.mapNotNull(::ed25519)
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
