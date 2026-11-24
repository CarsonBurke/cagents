package dev.cagents.mobile.android.ui

import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation3.runtime.NavKey
import androidx.navigation3.runtime.entryProvider
import androidx.navigation3.runtime.rememberNavBackStack
import androidx.navigation3.ui.NavDisplay
import kotlinx.serialization.Serializable

@Serializable
data object SessionsRoute : NavKey

@Serializable
data class ConversationRoute(val session: String) : NavKey

@Serializable
data object SettingsRoute : NavKey

@Composable
fun CagentsApp(vm: MainViewModel = viewModel()) {
    val settings by vm.settings.collectAsStateWithLifecycle()
    val start: NavKey = if (settings.isComplete) SessionsRoute else SettingsRoute
    val backStack = rememberNavBackStack(start)
    val snackbar = remember { SnackbarHostState() }

    LaunchedEffect(vm) {
        vm.messages.collect { snackbar.showSnackbar(it) }
    }

    HostKeyDialogs(vm)

    NavDisplay(
        backStack = backStack,
        onBack = { if (backStack.size > 1) backStack.removeAt(backStack.lastIndex) },
        entryProvider = entryProvider {
            entry<SessionsRoute> {
                SessionsScreen(
                    vm = vm,
                    snackbar = snackbar,
                    onOpen = { backStack.add(ConversationRoute(it)) },
                    onSettings = { backStack.add(SettingsRoute) },
                )
            }
            entry<ConversationRoute> { route ->
                ConversationScreen(
                    vm = vm,
                    session = route.session,
                    snackbar = snackbar,
                    onBack = { backStack.removeAt(backStack.lastIndex) },
                )
            }
            entry<SettingsRoute> {
                SettingsScreen(
                    vm = vm,
                    snackbar = snackbar,
                    canGoBack = backStack.size > 1,
                    onBack = { backStack.removeAt(backStack.lastIndex) },
                    onConnected = {
                        // From first-run setup, saving leads on to the sessions.
                        if (backStack.size == 1) {
                            backStack.add(SessionsRoute)
                            backStack.removeAt(0)
                        } else {
                            backStack.removeAt(backStack.lastIndex)
                        }
                    },
                )
            }
        },
    )
}
