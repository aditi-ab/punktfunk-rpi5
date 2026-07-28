package io.unom.punktfunk

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.AssistChip
import androidx.compose.material3.AssistChipDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

/**
 * The scope switcher: the one new settings concept. Selecting a profile puts the WHOLE settings
 * surface into that profile's scope — there is one settings UI, never a second parallel editor
 * that drifts from it. "Default settings" is the base layer every profile inherits from.
 *
 * A chips row rather than a menu, because on touch the scopes are worth seeing at a glance and
 * there are rarely more than a handful. The overflow beside them manages the SELECTED profile
 * (rename / duplicate / delete); with no profiles at all the row is just "Default settings" and a
 * "New profile" chip, which is all the clutter a user who never wants this feature ever sees.
 */
@Composable
internal fun ProfileScopeChips(
    profiles: List<StreamProfile>,
    selectedId: String?,
    onSelect: (String?) -> Unit,
    onNew: () -> Unit,
    onRename: () -> Unit,
    onDuplicate: () -> Unit,
    onDelete: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Row(
        modifier = modifier.horizontalScroll(rememberScrollState()).padding(horizontal = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        FilterChip(
            selected = selectedId == null,
            onClick = { onSelect(null) },
            label = { Text("Default settings") },
        )
        profiles.forEach { p ->
            FilterChip(
                selected = selectedId == p.id,
                onClick = { onSelect(p.id) },
                label = { Text(p.name) },
                leadingIcon = p.accent?.let { accent ->
                    { AccentDot(accentColor(accent) ?: MaterialTheme.colorScheme.primary) }
                },
            )
        }
        AssistChip(
            onClick = onNew,
            label = { Text("New profile") },
            leadingIcon = {
                Icon(Icons.Filled.Add, contentDescription = null, Modifier.size(AssistChipDefaults.IconSize))
            },
        )
        if (selectedId != null) {
            var menu by remember { mutableStateOf(false) }
            Box {
                IconButton(onClick = { menu = true }) {
                    Icon(Icons.Filled.MoreVert, contentDescription = "Manage profile")
                }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    DropdownMenuItem(text = { Text("Rename…") }, onClick = { menu = false; onRename() })
                    DropdownMenuItem(text = { Text("Duplicate") }, onClick = { menu = false; onDuplicate() })
                    DropdownMenuItem(text = { Text("Delete…") }, onClick = { menu = false; onDelete() })
                }
            }
        }
    }
}

/**
 * Name a profile — used by both "New profile…" and "Rename…". Names must be unique
 * case-insensitively: two "Work" chips in a menu are ambiguous, and a `punktfunk://…?profile=Work`
 * link would have to refuse rather than guess. [taken] is the live duplicate check, which lets a
 * rename keep its own name (and change only its case).
 */
@Composable
internal fun ProfileNameDialog(
    title: String,
    initialName: String,
    confirmLabel: String,
    taken: (String) -> Boolean,
    onConfirm: (String) -> Unit,
    onDismiss: () -> Unit,
) {
    var name by remember { mutableStateOf(initialName) }
    val trimmed = name.trim()
    val duplicate = trimmed.isNotEmpty() && taken(trimmed)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            Column {
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("Name") },
                    placeholder = { Text("e.g. Game, Work, Travel") },
                    singleLine = true,
                    isError = duplicate,
                )
                Text(
                    if (duplicate) {
                        "A profile called “$trimmed” already exists."
                    } else {
                        "A profile starts out inheriting every default setting. Whatever you " +
                            "change while it's selected becomes an override; everything else " +
                            "keeps following the defaults."
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = if (duplicate) {
                        MaterialTheme.colorScheme.error
                    } else {
                        MaterialTheme.colorScheme.onSurfaceVariant
                    },
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = trimmed.isNotEmpty() && !duplicate,
                onClick = { onConfirm(trimmed) },
            ) { Text(confirmLabel) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/**
 * Deleting a profile is not destructive to anything but the profile — a host bound to it falls
 * back to the default settings and a card pinned to it disappears, neither of which is an error.
 * The warning counts both so the consequence is stated rather than discovered.
 */
@Composable
internal fun DeleteProfileDialog(
    profile: StreamProfile,
    boundHosts: Int,
    pinnedCards: Int,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Delete “${profile.name}”?") },
        text = {
            Column {
                val consequences = buildList {
                    if (boundHosts > 0) {
                        add("$boundHosts ${plural(boundHosts, "host", "hosts")} will fall back to the default settings")
                    }
                    if (pinnedCards > 0) {
                        add("$pinnedCards pinned ${plural(pinnedCards, "card", "cards")} will disappear")
                    }
                }
                Text(
                    if (consequences.isEmpty()) {
                        "Nothing uses this profile."
                    } else {
                        consequences.joinToString(", and ") + "."
                    },
                )
                Text(
                    "The settings it overrides aren't lost anywhere else — the defaults stay " +
                        "exactly as they are.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        confirmButton = { TextButton(onClick = onConfirm) { Text("Delete") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

private fun plural(n: Int, one: String, many: String) = if (n == 1) one else many

/**
 * The per-host half of profiles, inside the host's Edit sheet: which profile a plain tap uses
 * (the binding — the one thing that IS sticky; "Connect with ▸" on a card never rebinds), and which
 * profiles get their own card in the host list.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun HostProfileBinding(
    profiles: List<StreamProfile>,
    boundId: String?,
    onBind: (String?) -> Unit,
    pins: List<String>,
    onTogglePin: (String) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val bound = profiles.firstOrNull { it.id == boundId }
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { expanded = it }) {
            OutlinedTextField(
                value = bound?.name ?: "Default settings",
                onValueChange = {},
                readOnly = true,
                label = { Text("Profile") },
                trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded) },
                modifier = Modifier
                    .menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable)
                    .fillMaxWidth(),
            )
            ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
                DropdownMenuItem(
                    text = { Text("Default settings") },
                    onClick = { onBind(null); expanded = false },
                )
                profiles.forEach { p ->
                    DropdownMenuItem(
                        text = { Text(p.name) },
                        onClick = { onBind(p.id); expanded = false },
                    )
                }
            }
        }
        Text(
            "What a tap on this host connects with.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            "Pinned cards",
            style = MaterialTheme.typography.labelLarge,
            modifier = Modifier.padding(top = 8.dp),
        )
        Text(
            "A pinned profile gets its own card beside this host — one tap instead of a menu. " +
                "Pinning changes nothing about which profile is the default.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        profiles.forEach { p ->
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(p.name, modifier = Modifier.weight(1f))
                Checkbox(checked = p.id in pins, onCheckedChange = { onTogglePin(p.id) })
            }
        }
    }
}

/** The accent marker a profile's chip and its pinned cards wear. */
@Composable
internal fun AccentDot(color: Color, size: Int = 10) {
    Box(Modifier.size(size.dp).clip(CircleShape).background(color))
}

/** `#RRGGBB` → a Compose colour, or null when the stored string isn't one (never a crash). */
internal fun accentColor(hex: String?): Color? {
    val h = hex?.removePrefix("#") ?: return null
    if (h.length != 6 || !h.all { it.isDigit() || it.lowercaseChar() in 'a'..'f' }) return null
    return runCatching { Color(h.toLong(16) or 0xFF000000L) }.getOrNull()
}
