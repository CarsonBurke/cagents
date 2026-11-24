package dev.cagents.mobile.android

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import dev.cagents.mobile.android.ui.CagentsApp
import dev.cagents.mobile.android.ui.CagentsTheme
import dev.cagents.mobile.android.ui.MainViewModel
import dev.cagents.mobile.pairing.PairingCode

class MainActivity : ComponentActivity() {
    private val vm: MainViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        // A recreated activity has already shown its link.
        if (savedInstanceState == null) offerPairingLink(intent)
        setContent {
            CagentsTheme {
                CagentsApp(vm)
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        offerPairingLink(intent)
    }

    /** A pairing code another app opened, such as the system camera's QR reader. It is confirmed before use. */
    private fun offerPairingLink(intent: Intent?) {
        val link = intent?.takeIf { it.action == Intent.ACTION_VIEW }?.dataString ?: return
        if (link.startsWith("${PairingCode.SCHEME}:", ignoreCase = true)) vm.openPairingLink(link)
    }
}
