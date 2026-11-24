package dev.cagents.mobile.android.ssh

import dev.cagents.mobile.android.data.ConnectionSettings
import dev.cagents.mobile.android.data.SettingsStore
import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.pairing.PairOutcome
import dev.cagents.mobile.pairing.PairingCode
import dev.cagents.mobile.pairing.PairingExchange
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.withContext
import java.io.IOException

/**
 * Pairs this phone with the computer that showed a [PairingCode]: connects
 * with the code's one-time key, trusting only the host key the code names,
 * asks the computer to authorize the phone's own key, and on success makes
 * that computer the one the app connects to.
 */
class Pairing(
    private val phoneKey: () -> PhoneKey,
    private val deviceName: () -> String,
    private val settings: SettingsStore,
    private val hostKeys: HostKeyStore,
) {
    /**
     * Runs the exchange, calling [onWaiting] once the computer is asking the
     * person. Cancelling it closes the connection, which withdraws the request.
     */
    suspend fun pair(code: PairingCode, onWaiting: () -> Unit): PairOutcome {
        val key = withContext(Dispatchers.IO) { phoneKey() }
        val channel = try {
            openSsh(
                SshTarget(
                    name = code.name,
                    hosts = code.hosts,
                    port = code.port,
                    user = code.user,
                    key = OneTimeKey(code.seed),
                    hostKeys = CodeHostKey(code),
                    // The one-time key's forced command runs whatever this says.
                    command = "cagents daemon bridge --pair",
                    unauthorized = {
                        ConnectFailure.Refused("${code.name} didn't accept this pairing code. It may have expired or been used: show a new one there.")
                    },
                ),
            )
        } catch (e: ConnectFailure) {
            return PairOutcome.Failed(e.message ?: "Couldn't reach ${code.name}.")
        }
        val outcome = try {
            PairingExchange.pair(channel, key.openSshKey, deviceName(), onWaiting)
        } catch (e: ConnectFailure) {
            PairOutcome.Failed(e.message ?: "The connection to ${code.name} was lost.")
        } catch (e: IOException) {
            PairOutcome.Failed("The connection to ${code.name} was lost. Show a new code there and try again.")
        } finally {
            // Disconnecting may write to the network.
            withContext(NonCancellable + Dispatchers.IO) { channel.close() }
        }
        if (outcome is PairOutcome.Paired) adopt(code)
        return outcome
    }

    /** Saves the computer as the one to connect to, its host key pinned for every address. */
    private fun adopt(code: PairingCode) {
        val adopted = ConnectionSettings(hosts = code.hosts, port = code.port, user = code.user, name = code.name)
        hostKeys.pin(adopted.hostKeyIds, code.hostKey, HostKeyStore.ED25519)
        settings.save(adopted)
    }
}

/**
 * Trusts exactly the host key the code names, never one seen for the first
 * time, and offers only Ed25519 host keys so that its fingerprint applies.
 */
private class CodeHostKey(private val code: PairingCode) : HostKeyCheck {
    override val ed25519Only get() = true

    override fun check(host: String, fingerprint: String, keyType: String): ConnectFailure? =
        if (keyType == HostKeyStore.ED25519 && fingerprint == code.hostKey) {
            null
        } else {
            // Not the computer that showed the code, perhaps another machine at that address.
            ConnectFailure.Refused(
                "The computer at $host isn't the one showing this code: its identity doesn't match. " +
                    "Check that this phone is on the computer's network or on Tailscale.",
            )
        }

    override fun preferred(host: String) = listOf(HostKeyStore.ED25519)
}
