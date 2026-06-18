package io.unom.punktfunk

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.ExperimentalAnimationApi
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import io.unom.punktfunk.models.Tab

@Composable
fun App() {
    val context = LocalContext.current
    val settingsStore = remember { SettingsStore(context) }
    var settings by remember { mutableStateOf(settingsStore.load()) }
    var streamHandle by remember { mutableLongStateOf(0L) } // 0 = not streaming
    var tab by remember { mutableStateOf(Tab.Connect) }

    AnimatedContent(
        targetState = streamHandle != 0L,
        transitionSpec = {
            fadeIn() togetherWith fadeOut()
        },
        label = "StreamTransition"
    ) { isStreaming ->
        if (isStreaming) {
            // Immersive: the stream takes the whole screen, no bottom bar.
            StreamScreen(streamHandle, micEnabled = settings.micEnabled, onDisconnect = { streamHandle = 0L })
        } else {
            Scaffold(
                bottomBar = {
                    NavigationBar {
                        Tab.entries.forEach { t ->
                            NavigationBarItem(
                                selected = tab == t,
                                onClick = { tab = t },
                                icon = { Icon(t.icon, contentDescription = t.label) },
                                label = { Text(t.label) },
                            )
                        }
                    }
                },
            ) { innerPadding ->
                Box(Modifier.fillMaxSize().padding(innerPadding)) {
                    AnimatedContent(
                        targetState = tab,
                        transitionSpec = {
                            if (targetState.ordinal > initialState.ordinal) {
                                slideInHorizontally { it } + fadeIn() togetherWith
                                        slideOutHorizontally { -it } + fadeOut()
                            } else {
                                slideInHorizontally { -it } + fadeIn() togetherWith
                                        slideOutHorizontally { it } + fadeOut()
                            }
                        },
                        label = "TabTransition"
                    ) { targetTab ->
                        when (targetTab) {
                            Tab.Connect -> ConnectScreen(settings = settings, onConnected = { streamHandle = it })
                            Tab.Settings -> SettingsScreen(
                                initial = settings,
                                onChange = { settings = it; settingsStore.save(it) },
                                onBack = { tab = Tab.Connect },
                            )
                        }
                    }
                }
            }
        }
    }
}
