package dev.cagents.mobile.android.data

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/** Where the computer is and how to start the bridge there. */
data class ConnectionSettings(
    /**
     * The computer's addresses, in the order to try them. A pairing code
     * gives its Tailscale name and address first, then local ones.
     */
    val hosts: List<String> = emptyList(),
    val port: Int = 22,
    val user: String = "",
    /**
     * The remote command. A key restricted with `command=` in
     * authorized_keys runs that command whatever this says.
     */
    val command: String = DEFAULT_COMMAND,
    /** What to call the computer, from its pairing code; empty when set up by hand. */
    val name: String = "",
) {
    /** The preferred address. */
    val host: String get() = hosts.firstOrNull().orEmpty()

    /** How to refer to the computer. */
    val label: String get() = name.ifBlank { host }

    val isComplete: Boolean get() = hosts.isNotEmpty() && user.isNotBlank() && port in 1..65535

    /** Which pinned host key applies to [host]. */
    fun hostKeyId(host: String): String = "${host.trim().lowercase()}:$port"

    val hostKeyIds: List<String> get() = hosts.map(::hostKeyId)

    companion object {
        const val DEFAULT_COMMAND = "cagents daemon bridge --mobile"

        /** Addresses as typed: separated by commas or spaces. */
        fun parseHosts(text: String): List<String> =
            text.split(',', ' ', '\n').map(String::trim).filter(String::isNotEmpty).distinct()
    }
}

class SettingsStore(context: Context) {
    private val prefs = context.getSharedPreferences("connection", Context.MODE_PRIVATE)
    private val _settings = MutableStateFlow(read())
    val settings: StateFlow<ConnectionSettings> = _settings.asStateFlow()

    private fun read() = ConnectionSettings(
        // "host" alone is how settings were kept before there could be several.
        hosts = (prefs.getString("hosts", null) ?: prefs.getString("host", "")).orEmpty()
            .lines().map(String::trim).filter(String::isNotEmpty),
        port = prefs.getInt("port", 22),
        user = prefs.getString("user", "").orEmpty(),
        command = prefs.getString("command", null) ?: ConnectionSettings.DEFAULT_COMMAND,
        name = prefs.getString("name", "").orEmpty(),
    )

    fun save(settings: ConnectionSettings) {
        val clean = settings.copy(
            hosts = settings.hosts.map(String::trim).filter(String::isNotEmpty).distinct(),
            user = settings.user.trim(),
            command = settings.command.trim().ifEmpty { ConnectionSettings.DEFAULT_COMMAND },
            name = settings.name.trim(),
        )
        prefs.edit()
            .putString("hosts", clean.hosts.joinToString("\n"))
            .putString("host", clean.host)
            .putInt("port", clean.port)
            .putString("user", clean.user)
            .putString("command", clean.command)
            .putString("name", clean.name)
            .apply()
        _settings.value = clean
    }
}
