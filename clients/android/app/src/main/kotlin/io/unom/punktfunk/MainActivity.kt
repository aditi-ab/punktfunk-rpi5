package io.unom.punktfunk

import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.NativeBridge

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Cross the JNI bridge into libpunktfunk_android.so → punktfunk-core. A live ABI version is
        // the scaffold's proof the whole native stack is wired (cargo-ndk → jniLibs → APK →
        // System.loadLibrary → JNI → core). Logged so it's verifiable headlessly via logcat.
        val abi = runCatching { NativeBridge.abiVersion() }.getOrDefault(-1)
        val core = runCatching { NativeBridge.coreVersion() }.getOrDefault("?")
        Log.i("punktfunk", "native bridge: core ABI v$abi, core $core")

        enableEdgeToEdge()
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(modifier = Modifier.fillMaxSize()) {
                    ScaffoldScreen(abi, core)
                }
            }
        }
    }
}

@Composable
private fun ScaffoldScreen(abi: Int, core: String) {
    Column(
        modifier = Modifier.fillMaxSize().padding(24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Text("punktfunk", style = MaterialTheme.typography.headlineMedium)
        Text("Android client — scaffold", style = MaterialTheme.typography.bodyMedium)
        Text(
            if (abi > 0) "✓ native bridge linked" else "✗ native bridge FAILED",
            style = MaterialTheme.typography.titleMedium,
        )
        Text("core ABI v$abi · core $core", style = MaterialTheme.typography.bodySmall)
    }
}
