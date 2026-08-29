package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.ArrowDropDown
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp

/**
 * The quick-action ring's editor (design/touch-client-overlay.md §3.3, as a list): which action
 * sits in each of the six slots, the custom shortcuts a slot can send, and the way back to the
 * platform default. A deep sub-screen of Settings like [ControllersScreen]; [blob] is the
 * `overlay_actions` of the layer being edited and [onChange] writes it back through the same
 * `update` every row uses, so a profile that touches it owns the whole ring (D10).
 */
@Composable
internal fun QuickActionsScreen(
    blob: String,
    onChange: (String) -> Unit,
    onReset: () -> Unit,
    onBack: () -> Unit,
) {
    BackHandler(onBack = onBack)
    val cfg = remember(blob) { OverlayConfig.parse(blob) }
    var adding by remember { mutableStateOf(false) }
    val options = BUILTIN_SLOTS + cfg.shortcuts.map {
        SlotOption("shortcut:${it.id}", it.label.ifEmpty { chordChip(it.keys) })
    }

    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalArrangement = Arrangement.spacedBy(20.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onBack, modifier = Modifier.padding(end = 4.dp)) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Text("Quick actions", style = MaterialTheme.typography.headlineMedium)
        }
        SettingsGroup("Ring") {
            cfg.ring.forEachIndexed { k, slot ->
                PickerRow("${CLOCK[k]} o'clock", options, slot?.id ?: "") { id ->
                    val ring = cfg.ring.toMutableList().also { it[k] = SlotId.parse(id) }
                    onChange(cfg.copy(ring = ring).toJson())
                }
            }
        }
        SettingsGroup("Shortcuts", footer = "A new shortcut takes the first empty slot.") {
            cfg.shortcuts.forEach { sc ->
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        sc.label.ifEmpty { chordChip(sc.keys) },
                        style = MaterialTheme.typography.bodyLarge,
                        modifier = Modifier.weight(1f),
                    )
                    Text(
                        chordChip(sc.keys),
                        style = MaterialTheme.typography.bodyMedium,
                        fontFamily = FontFamily.Monospace,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    IconButton(onClick = { onChange(remove(cfg, sc.id).toJson()) }) {
                        Icon(Icons.Filled.Delete, contentDescription = "Remove ${sc.label}")
                    }
                }
            }
            if (adding) {
                AddShortcut(onCancel = { adding = false }) { label, keys ->
                    onChange(add(cfg, label, keys).toJson())
                    adding = false
                }
            } else {
                TextButton(onClick = { adding = true }) { Text("Add shortcut") }
            }
        }
        SettingsGroup(footer = "Restores the platform ring and removes the shortcuts.") {
            TextButton(onClick = onReset) {
                Text("Reset to default", color = MaterialTheme.colorScheme.error)
            }
        }
    }
}

private data class SlotOption(val id: String, val label: String)

/**
 * What a slot can hold, in picker order: empty, the built-ins, the three host power actions by
 * their advertised ids, then (appended per config) the profile's own shortcuts.
 */
private val BUILTIN_SLOTS = listOf(
    SlotOption("", "Empty"),
    SlotOption("end_stream", "End stream"),
    SlotOption("disconnect_linger", "Disconnect, keep the game running"),
    SlotOption("touch_mode", "Touch mode"),
    SlotOption("keyboard", "Keyboard"),
    SlotOption("stats", "Statistics"),
    SlotOption("mic", "Microphone"),
    SlotOption("pad", "Virtual controller"),
    SlotOption("send_text", "Send text"),
    SlotOption("host:power.sleep", "Host: sleep"),
    SlotOption("host:power.reboot", "Host: reboot"),
    SlotOption("host:power.shutdown", "Host: shut down"),
)

private val MODIFIER_KEYS = listOf("ctrl", "alt", "shift", "win")

/** The keys a chord can end on — names [keyVk] knows. */
private val CHORD_KEYS: List<String> =
    listOf(
        "escape", "tab", "enter", "space", "backspace", "delete", "insert", "home", "end",
        "pageup", "pagedown", "up", "down", "left", "right", "printscreen", "pause",
    ) + (1..12).map { "f$it" } + ('a'..'z').map { it.toString() } + (0..9).map { it.toString() }

private val CLOCK = listOf("12", "2", "4", "6", "8", "10")

private fun add(cfg: OverlayConfig, label: String, keys: List<String>): OverlayConfig {
    val next = (cfg.shortcuts.mapNotNull { it.id.drop(1).toIntOrNull() }.maxOrNull() ?: 0) + 1
    val sc = Shortcut("s$next", label, keys)
    val ring = cfg.ring.toMutableList()
    ring.indexOf(null).takeIf { it >= 0 }?.let { ring[it] = SlotId.Shortcut(sc.id) }
    return cfg.copy(ring = ring, shortcuts = cfg.shortcuts + sc)
}

private fun remove(cfg: OverlayConfig, id: String): OverlayConfig = cfg.copy(
    // `parse` would empty a dangling slot on the next read; write it empty now so the picker
    // shows it at once.
    ring = cfg.ring.map { if (it is SlotId.Shortcut && it.shortcutId == id) null else it },
    shortcuts = cfg.shortcuts.filter { it.id != id },
)

/** A label on the left, the picked option on the right; the whole row opens the menu. */
@Composable
private fun PickerRow(
    label: String,
    options: List<SlotOption>,
    selected: String,
    onSelect: (String) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    Box {
        Row(
            modifier = Modifier.fillMaxWidth().clickable { expanded = true },
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(label, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
            Text(
                options.firstOrNull { it.id == selected }?.label ?: "Empty",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Icon(
                Icons.Filled.ArrowDropDown,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            options.forEach { o ->
                DropdownMenuItem(text = { Text(o.label) }, onClick = {
                    onSelect(o.id)
                    expanded = false
                })
            }
        }
    }
}

/** One chord: a label, the modifiers held, and the key it ends on. */
@Composable
private fun AddShortcut(onCancel: () -> Unit, onAdd: (label: String, keys: List<String>) -> Unit) {
    var label by remember { mutableStateOf("") }
    var mods by remember { mutableStateOf(setOf<String>()) }
    var key by remember { mutableStateOf("escape") }
    val keys = MODIFIER_KEYS.filter { it in mods } + key
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        OutlinedTextField(
            value = label,
            onValueChange = { label = it },
            label = { Text("Label (optional)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            MODIFIER_KEYS.forEach { m ->
                FilterChip(
                    selected = m in mods,
                    onClick = { mods = if (m in mods) mods - m else mods + m },
                    label = { Text(chordChip(listOf(m))) },
                )
            }
        }
        PickerRow("Key", CHORD_KEYS.map { SlotOption(it, chordChip(listOf(it))) }, key) { key = it }
        Text(
            chordChip(keys),
            style = MaterialTheme.typography.bodyLarge,
            fontFamily = FontFamily.Monospace,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            TextButton(onClick = onCancel) { Text("Cancel") }
            Button(onClick = { onAdd(label, keys) }) { Text("Add") }
        }
    }
}
