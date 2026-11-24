package dev.cagents.mobile.android.ui

import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

// The desktop app's coral, for phones without dynamic colour.
private val Coral = Color(0xFFEE5A24)

private val LightFallback = lightColorScheme(
    primary = Color(0xFFA63B10),
    onPrimary = Color.White,
    primaryContainer = Color(0xFFFFDBCF),
    onPrimaryContainer = Color(0xFF390C00),
    secondary = Color(0xFF2E6B62),
    secondaryContainer = Color(0xFFB4EFE3),
    onSecondaryContainer = Color(0xFF00201C),
)

private val DarkFallback = darkColorScheme(
    primary = Color(0xFFFFB59C),
    onPrimary = Color(0xFF5D1900),
    primaryContainer = Coral,
    onPrimaryContainer = Color.White,
    secondary = Color(0xFF8DD3C6),
    secondaryContainer = Color(0xFF145249),
    onSecondaryContainer = Color(0xFFB4EFE3),
)

/** Colours for states Material's scheme has no role for. */
@Immutable
data class StatusColors(val attention: Color, val onAttention: Color, val working: Color, val success: Color)

val LocalStatusColors = staticCompositionLocalOf {
    StatusColors(Color(0xFFB26A00), Color.White, Color(0xFF2E6B62), Color(0xFF2E7D32))
}

@Composable
fun CagentsTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    val context = LocalContext.current
    val scheme = when {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S ->
            if (dark) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        dark -> DarkFallback
        else -> LightFallback
    }
    val status = if (dark) {
        StatusColors(Color(0xFFFFB952), Color(0xFF452B00), scheme.tertiary, Color(0xFF8CD48F))
    } else {
        StatusColors(Color(0xFF8A5100), Color.White, scheme.tertiary, Color(0xFF2E7D32))
    }
    androidx.compose.runtime.CompositionLocalProvider(LocalStatusColors provides status) {
        MaterialTheme(colorScheme = scheme, content = content)
    }
}
