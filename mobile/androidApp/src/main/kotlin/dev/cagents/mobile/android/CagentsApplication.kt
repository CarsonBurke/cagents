package dev.cagents.mobile.android

import android.app.Application
import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.provider.Settings as AndroidSettings
import android.util.Log
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import dev.cagents.mobile.android.data.SettingsStore
import dev.cagents.mobile.android.ssh.HostKeyStore
import dev.cagents.mobile.android.ssh.Pairing
import dev.cagents.mobile.android.ssh.PhoneKey
import dev.cagents.mobile.android.ssh.SshBridgeTransport
import dev.cagents.mobile.client.ControllerConfig
import dev.cagents.mobile.client.RemoteController
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import org.bouncycastle.jce.provider.BouncyCastleProvider
import java.io.File
import java.security.Security

class CagentsApplication : Application() {
    lateinit var graph: AppGraph
        private set

    override fun onCreate() {
        super.onCreate()
        installBouncyCastle()
        graph = AppGraph(this)
    }

    /**
     * sshj looks BouncyCastle up by the name "BC", which Android gives to a
     * cut-down copy without the curves SSH needs. Replace it with the full
     * library, last in line, so the platform keeps serving everything else,
     * including signing with Keystore keys.
     */
    private fun installBouncyCastle() {
        Security.removeProvider(BouncyCastleProvider.PROVIDER_NAME)
        Security.addProvider(BouncyCastleProvider())
    }
}

val Context.graph: AppGraph get() = (applicationContext as CagentsApplication).graph

/** The app's long-lived objects. */
class AppGraph(private val app: Application) {
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    val settings = SettingsStore(app)
    val hostKeys = HostKeyStore(app)
    val controller = RemoteController(
        scope = scope,
        transport = SshBridgeTransport(
            phoneKey = { PhoneKey.get(app) },
            settings = { settings.settings.value },
            hostKeys = hostKeys,
        ),
        config = ControllerConfig(clientVersion = BuildConfig.VERSION_NAME),
    )
    val pairing = Pairing(
        phoneKey = { PhoneKey.get(app) },
        deviceName = ::deviceName,
        settings = settings,
        hostKeys = hostKeys,
    )

    /** What the computer will call this phone: the name it was given in system settings, or its model. */
    private fun deviceName(): String =
        AndroidSettings.Global.getString(app.contentResolver, AndroidSettings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() }
            ?: Build.MODEL

    init {
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) {
                if (settings.settings.value.isComplete) controller.setForeground(true)
            }

            override fun onStop(owner: LifecycleOwner) = controller.setForeground(false)
        })

        // A network coming back is worth more than the rest of a backoff.
        app.getSystemService(ConnectivityManager::class.java)?.registerDefaultNetworkCallback(
            object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    if (ProcessLifecycleOwner.get().lifecycle.currentState.isAtLeast(
                            androidx.lifecycle.Lifecycle.State.STARTED,
                        )
                    ) {
                        controller.retryNow()
                    }
                }
            },
        )

        if (BuildConfig.DEBUG) exportPublicKeyForDevelopment()
    }

    /**
     * Debug builds leave the public key where `adb shell run-as` can read it,
     * so a developer can authorize the phone without retyping it.
     */
    private fun exportPublicKeyForDevelopment() = scope.launch(Dispatchers.IO) {
        runCatching {
            val key = PhoneKey.get(app)
            File(app.filesDir, "cagents-phone.pub").writeText(key.openSsh + "\n")
            Log.i("cagents", "Phone key (hardware-backed: ${key.hardwareBacked}): ${key.openSsh}")
        }.onFailure { Log.w("cagents", "Couldn't make the phone's key", it) }
    }
}
