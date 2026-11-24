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
import androidx.navigation3.runtime.rememberSaveableStateHolderNavEntryDecorator
import androidx.lifecycle.viewmodel.navigation3.rememberViewModelStoreNavEntryDecorator
import kotlinx.serialization.Serializable

@Serializable
data object SessionsRoute : NavKey

@Serializable
data class ConversationRoute(val session: String) : NavKey

@Serializable
data object SettingsRoute : NavKey

/** Pairing with a computer: scanning its code, or confirming [link], one another app opened. */
@Serializable
data class PairingRoute(val link: String? = null) : NavKey

@Composable
fun CagentsApp(vm: MainViewModel = viewModel()) {
    val settings by vm.settings.collectAsStateWithLifecycle()
    val start: NavKey = if (settings.isComplete) SessionsRoute else SettingsRoute
    val backStack = rememberNavBackStack(start)
    val snackbar = remember { SnackbarHostState() }

    LaunchedEffect(vm) {
        vm.messages.collect { snackbar.showSnackbar(it) }
    }

    val pairingLink by vm.pairingLink.collectAsStateWithLifecycle()
    LaunchedEffect(pairingLink) {
        val link = pairingLink ?: return@LaunchedEffect
        vm.openPairingLink(null)
        // One pairing at a time: a newer link replaces a pairing screen already open.
        backStack.removeAll { it is PairingRoute }
        if (backStack.isEmpty()) backStack.add(SettingsRoute)
        backStack.add(PairingRoute(link))
    }

    HostKeyDialogs(vm)

    NavDisplay(
        backStack = backStack,
        onBack = { if (backStack.size > 1) backStack.removeAt(backStack.lastIndex) },
        // Each screen keeps its own saved state and ViewModels until it leaves the back stack.
        entryDecorators = listOf(
            rememberSaveableStateHolderNavEntryDecorator(),
            rememberViewModelStoreNavEntryDecorator(),
        ),
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
                    onScan = { backStack.add(PairingRoute()) },
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
            entry<PairingRoute> { route ->
                PairingScreen(
                    main = vm,
                    link = route.link,
                    onBack = { backStack.removeAt(backStack.lastIndex) },
                    onPaired = {
                        // Paired: on to the new computer's sessions, with nothing to go back to.
                        backStack.add(SessionsRoute)
                        while (backStack.size > 1) backStack.removeAt(0)
                        vm.showManualSetup(false)
                    },
                    onManual = {
                        vm.showManualSetup()
                        backStack.removeAt(backStack.lastIndex)
                        if (backStack.lastOrNull() != SettingsRoute) backStack.add(SettingsRoute)
                    },
                )
            }
        },
    )
}
