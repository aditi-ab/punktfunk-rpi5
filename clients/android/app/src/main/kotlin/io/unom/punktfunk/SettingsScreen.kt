package io.unom.punktfunk

import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat

/**
 * Stream settings. Edits are persisted immediately via [onChange]; [onBack] returns to the connect
 * screen. Resolution/refresh "Native" resolve from the device display at connect time.
 */
@Composable
fun SettingsScreen(initial: Settings, onChange: (Settings) -> Unit, onBack: () -> Unit) {
    var s by remember { mutableStateOf(initial) }
    val context = LocalContext.current
    fun update(next: Settings) {
        s = next
        onChange(next)
    }

    BackHandler(onBack = onBack)

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("Settings", style = MaterialTheme.typography.headlineMedium)

        val (nw, nh, nhz) = nativeDisplayMode(context)
        SettingDropdown(
            label = "Resolution",
            options = RESOLUTION_OPTIONS.map { (w, h, lbl) ->
                (w to h) to (if (w == 0) "$lbl ($nw × $nh)" else lbl)
            },
            selected = s.width to s.height,
        ) { (w, h) -> update(s.copy(width = w, height = h)) }

        SettingDropdown(
            label = "Refresh rate",
            options = REFRESH_OPTIONS.map { (hz, lbl) -> hz to (if (hz == 0) "$lbl (${nhz} Hz)" else lbl) },
            selected = s.hz,
        ) { hz -> update(s.copy(hz = hz)) }

        SettingDropdown(
            label = "Bitrate",
            options = BITRATE_OPTIONS,
            selected = s.bitrateKbps,
        ) { kbps -> update(s.copy(bitrateKbps = kbps)) }

        SettingDropdown(
            label = "Compositor (virtual-display host backend)",
            options = COMPOSITOR_OPTIONS.mapIndexed { i, lbl -> i to lbl },
            selected = s.compositor,
        ) { c -> update(s.copy(compositor = c)) }

        SettingDropdown(
            label = "Controller type",
            options = GAMEPAD_OPTIONS.mapIndexed { i, lbl -> i to lbl },
            selected = s.gamepad,
        ) { g -> update(s.copy(gamepad = g)) }

        // Mic uplink — turning it on requests RECORD_AUDIO; if denied, the toggle stays off.
        val micLauncher = rememberLauncherForActivityResult(
            ActivityResultContracts.RequestPermission(),
        ) { granted -> update(s.copy(micEnabled = granted)) }
        Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Microphone", style = MaterialTheme.typography.bodyLarge)
                Text(
                    "Send your mic to the host's virtual microphone",
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            Switch(
                checked = s.micEnabled,
                onCheckedChange = { on ->
                    when {
                        !on -> update(s.copy(micEnabled = false))
                        ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) ==
                            PackageManager.PERMISSION_GRANTED -> update(s.copy(micEnabled = true))
                        else -> micLauncher.launch(Manifest.permission.RECORD_AUDIO)
                    }
                },
            )
        }
    }
}

/** A labelled read-only dropdown over [options] (value → label); calls [onSelect] on a pick. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun <T> SettingDropdown(
    label: String,
    options: List<Pair<T, String>>,
    selected: T,
    onSelect: (T) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val selectedLabel = options.firstOrNull { it.first == selected }?.second
        ?: options.firstOrNull()?.second.orEmpty()
    ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { expanded = it }) {
        OutlinedTextField(
            value = selectedLabel,
            onValueChange = {},
            readOnly = true,
            label = { Text(label) },
            trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded) },
            modifier = Modifier
                .menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable)
                .fillMaxWidth(),
        )
        ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            options.forEach { (value, lbl) ->
                DropdownMenuItem(
                    text = { Text(lbl) },
                    onClick = {
                        onSelect(value)
                        expanded = false
                    },
                )
            }
        }
    }
}
