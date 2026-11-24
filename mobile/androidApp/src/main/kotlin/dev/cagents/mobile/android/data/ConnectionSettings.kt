package dev.cagents.mobile.android.data

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/** Where the computer is and how to start the bridge there. */
data class ConnectionSettings(
    val host: String = "",
    val port: Int = 22,
    val user: String = "",
    /**
     * The remote command. A key restricted with `command=` in
     * authorized_keys runs that command whatever this says.
     */
    val command: String = DEFAULT_COMMAND,
) {
    val isComplete: Boolean get() = host.isNotBlank() && user.isNotBlank() && port in 1..65535

    /** Which pinned host key applies. */
    val hostKeyId: String get() = "${host.trim().lowercase()}:$port"

    companion object {
        const val DEFAULT_COMMAND = "cagents daemon bridge --mobile"
    }
}

class SettingsStore(context: Context) {
    private val prefs = context.getSharedPreferences("connection", Context.MODE_PRIVATE)
    private val _settings = MutableStateFlow(read())
    val settings: StateFlow<ConnectionSettings> = _settings.asStateFlow()

    private fun read() = ConnectionSettings(
        host = prefs.getString("host", "").orEmpty(),
        port = prefs.getInt("port", 22),
        user = prefs.getString("user", "").orEmpty(),
        command = prefs.getString("command", null) ?: ConnectionSettings.DEFAULT_COMMAND,
    )

    fun save(settings: ConnectionSettings) {
        val clean = settings.copy(
            host = settings.host.trim(),
            user = settings.user.trim(),
            command = settings.command.trim().ifEmpty { ConnectionSettings.DEFAULT_COMMAND },
        )
        prefs.edit()
            .putString("host", clean.host)
            .putInt("port", clean.port)
            .putString("user", clean.user)
            .putString("command", clean.command)
            .apply()
        _settings.value = clean
    }
}
