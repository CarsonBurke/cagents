package dev.cagents.mobile.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import dev.cagents.mobile.android.ui.CagentsApp
import dev.cagents.mobile.android.ui.CagentsTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        setContent {
            CagentsTheme {
                CagentsApp()
            }
        }
    }
}
