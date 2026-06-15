package io.unom.punktfunk

import android.os.Bundle
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import io.unom.punktfunk.kit.NativeBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(modifier = Modifier.fillMaxSize()) { App() }
            }
        }
    }
}

/** Scaffold mode requested from the host (WxH@Hz). TODO: derive from the display. */
private val REQUEST_MODE = Triple(1280, 720, 60)

private sealed interface Screen {
    data object Connect : Screen
    data class Stream(val handle: Long) : Screen
}

@Composable
private fun App() {
    var screen by remember { mutableStateOf<Screen>(Screen.Connect) }
    when (val s = screen) {
        Screen.Connect -> ConnectScreen(onConnected = { handle -> screen = Screen.Stream(handle) })
        is Screen.Stream -> StreamScreen(s.handle, onDisconnect = { screen = Screen.Connect })
    }
}

@Composable
private fun ConnectScreen(onConnected: (Long) -> Unit) {
    val scope = rememberCoroutineScope()
    var host by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("9777") }
    var connecting by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf<String?>(null) }
    val abi = remember { runCatching { NativeBridge.abiVersion() }.getOrDefault(-1) }
    val (w, h, hz) = REQUEST_MODE

    Column(
        modifier = Modifier.fillMaxSize().padding(24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Text("punktfunk", style = MaterialTheme.typography.headlineMedium)
        Text("Android client", style = MaterialTheme.typography.bodyMedium)
        Spacer(Modifier.height(24.dp))
        OutlinedTextField(
            value = host,
            onValueChange = { host = it },
            label = { Text("Host") },
            singleLine = true,
        )
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = port,
            onValueChange = { v -> port = v.filter { it.isDigit() }.take(5) },
            label = { Text("Port") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        )
        Spacer(Modifier.height(16.dp))
        Button(
            enabled = !connecting && host.isNotBlank() && port.isNotBlank(),
            onClick = {
                connecting = true
                status = "Connecting to $host:$port…"
                scope.launch {
                    val handle = withContext(Dispatchers.IO) {
                        NativeBridge.nativeConnect(host.trim(), port.toInt(), w, h, hz)
                    }
                    connecting = false
                    if (handle != 0L) {
                        onConnected(handle)
                    } else {
                        status = "Connection failed — check host/port and logcat"
                    }
                }
            },
        ) { Text(if (connecting) "Connecting…" else "Connect  ($w×$h@$hz)") }
        status?.let {
            Spacer(Modifier.height(12.dp))
            Text(it, style = MaterialTheme.typography.bodySmall)
        }
        Spacer(Modifier.height(24.dp))
        Text("core ABI v$abi", style = MaterialTheme.typography.labelSmall)
    }
}

@Composable
private fun StreamScreen(handle: Long, onDisconnect: () -> Unit) {
    val context = LocalContext.current
    val window = (context as? ComponentActivity)?.window

    DisposableEffect(handle) {
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        onDispose {
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            // Leaving the stream: stop the decode thread and tear down the session.
            NativeBridge.nativeStopVideo(handle)
            NativeBridge.nativeClose(handle)
        }
    }

    BackHandler { onDisconnect() }

    AndroidView(
        modifier = Modifier.fillMaxSize(),
        factory = { ctx ->
            SurfaceView(ctx).apply {
                holder.addCallback(object : SurfaceHolder.Callback {
                    override fun surfaceCreated(holder: SurfaceHolder) {
                        NativeBridge.nativeStartVideo(handle, holder.surface)
                    }

                    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

                    override fun surfaceDestroyed(holder: SurfaceHolder) {
                        NativeBridge.nativeStopVideo(handle)
                    }
                })
            }
        },
    )
}
