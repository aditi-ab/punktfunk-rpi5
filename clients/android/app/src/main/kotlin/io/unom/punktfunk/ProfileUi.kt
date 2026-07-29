package io.unom.punktfunk

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.ArrowDropDown
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
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp

/**
 * The scope switcher: the one new settings concept. Selecting a profile puts the WHOLE settings
 * surface into that profile's scope — there is one settings UI, never a second parallel editor
 * that drifts from it. "Default settings" is the base layer every profile inherits from.
 *
 * A chips row rather than a menu, because on touch the scopes are worth seeing at a glance and
 * there are rarely more than a handful. Managing a profile lives ON its chip: the selected one
 * grows a chevron, and tapping it again opens Edit / Duplicate / Delete anchored under it. That
 * replaced a lone overflow button parked after the LAST chip — which meant scrolling past every
 * profile to reach an action that applied to one of them, with nothing on screen saying which.
 *
 * With no profiles at all the row is just "Default settings" and a "New profile" chip, which is
 * all the clutter a user who never wants this feature ever sees.
 */
@Composable
internal fun ProfileScopeChips(
    profiles: List<StreamProfile>,
    selectedId: String?,
    onSelect: (String?) -> Unit,
    onNew: () -> Unit,
    onEdit: (StreamProfile) -> Unit,
    onDuplicate: (StreamProfile) -> Unit,
    onDelete: (StreamProfile) -> Unit,
    modifier: Modifier = Modifier,
) {
    // Which chip's menu is open — one at a time, and it closes itself when the scope changes.
    var menuFor by remember { mutableStateOf<String?>(null) }
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
            val isSelected = selectedId == p.id
            Box {
                FilterChip(
                    selected = isSelected,
                    // Tap to select; tap the selected one — the one wearing the chevron — to manage
                    // it. The action is on the object it acts on, which is the whole point.
                    onClick = { if (isSelected) menuFor = p.id else onSelect(p.id) },
                    // The dot and the chevron ride INSIDE the label, not in the `leadingIcon` /
                    // `trailingIcon` slots: those reserve an 18dp icon and shrink the chip's padding
                    // to suit, so a chip with an accent (or with the chevron) would sit differently
                    // from "Default settings" beside it. In the label every chip keeps the same
                    // padding and the spacing is ours to set.
                    label = {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            accentColor(p.accent)?.let { dot ->
                                AccentDot(dot, size = 8)
                                Spacer(Modifier.width(8.dp))
                            }
                            Text(p.name)
                            if (isSelected) {
                                Spacer(Modifier.width(2.dp))
                                Icon(
                                    Icons.Filled.ArrowDropDown,
                                    contentDescription = "Manage “${p.name}”",
                                    modifier = Modifier.size(18.dp),
                                )
                            }
                        }
                    },
                )
                DropdownMenu(expanded = menuFor == p.id, onDismissRequest = { menuFor = null }) {
                    DropdownMenuItem(text = { Text("Edit…") }, onClick = { menuFor = null; onEdit(p) })
                    DropdownMenuItem(
                        text = { Text("Duplicate") },
                        onClick = { menuFor = null; onDuplicate(p) },
                    )
                    DropdownMenuItem(text = { Text("Delete…") }, onClick = { menuFor = null; onDelete(p) })
                }
            }
        }
        AssistChip(
            onClick = onNew,
            label = { Text("New profile") },
            leadingIcon = {
                Icon(Icons.Filled.Add, contentDescription = null, Modifier.size(AssistChipDefaults.IconSize))
            },
        )
    }
}

/**
 * Create or edit a profile: its name and its colour, decided together. They were two flows —
 * a name dialog at creation, "Change colour…" afterwards — which meant every profile started
 * colourless-looking until the user went hunting for a menu item, and the accent is exactly the
 * signal that has to be there from the first moment (it is all a bound host card's chip and a
 * pinned card's tint have to go on).
 *
 * Names must be unique case-insensitively: two "Work" chips in a menu are ambiguous, and a
 * `punktfunk://…?profile=Work` link would have to refuse rather than guess. [taken] is the live
 * duplicate check, which lets an edit keep its own name (and change only its case).
 */
@Composable
internal fun ProfileEditorDialog(
    title: String,
    confirmLabel: String,
    initialName: String,
    initialAccent: String?,
    creating: Boolean,
    taken: (String) -> Boolean,
    onConfirm: (name: String, accent: String?) -> Unit,
    onDismiss: () -> Unit,
) {
    var name by remember { mutableStateOf(initialName) }
    var accent by remember { mutableStateOf(initialAccent) }
    val trimmed = name.trim()
    val duplicate = trimmed.isNotEmpty() && taken(trimmed)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            ProfileEditorFields(
                name = name,
                accent = accent,
                duplicate = duplicate,
                creating = creating,
                onNameChange = { name = it },
                onAccentChange = { accent = it },
            )
        },
        confirmButton = {
            TextButton(
                enabled = trimmed.isNotEmpty() && !duplicate,
                onClick = { onConfirm(trimmed, accent) },
            ) { Text(confirmLabel) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/**
 * The editor's body. Extracted from [ProfileEditorDialog] so the screenshot harness can render
 * exactly these — a focused text field inside a Dialog window never reaches idle under Robolectric,
 * so the dialog itself is uncapturable, and an eyeballed-only layout is how this shipped once with
 * the field and its caption touching.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
internal fun ProfileEditorFields(
    name: String,
    accent: String?,
    duplicate: Boolean,
    creating: Boolean,
    onNameChange: (String) -> Unit,
    onAccentChange: (String?) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        OutlinedTextField(
            value = name,
            onValueChange = onNameChange,
            label = { Text("Name") },
            placeholder = { Text("e.g. Game, Work, Travel") },
            singleLine = true,
            isError = duplicate,
        )
        Text(
            when {
                duplicate -> "A profile called “${name.trim()}” already exists."
                creating -> "A profile starts out inheriting every default setting. Whatever you " +
                    "change while it's selected becomes an override."
                else -> "The colour marks this profile on host cards, where its name doesn't fit."
            },
            style = MaterialTheme.typography.bodySmall,
            color = if (duplicate) {
                MaterialTheme.colorScheme.error
            } else {
                MaterialTheme.colorScheme.onSurfaceVariant
            },
        )
        FlowRow(
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            PROFILE_ACCENTS.forEach { hex ->
                Swatch(
                    colour = accentColor(hex),
                    selected = accent?.equals(hex, ignoreCase = true) == true,
                    onClick = { onAccentChange(hex) },
                )
            }
            // "No colour" is a real choice, not only an initial state: the chip then falls back to
            // the theme's own accent, which is what a profile made before colours existed shows.
            Swatch(colour = null, selected = accent == null, onClick = { onAccentChange(null) })
        }
    }
}

/** One colour choice: a filled disc, ringed when it is the current one. `null` = no colour. */
@Composable
internal fun Swatch(colour: Color?, selected: Boolean, onClick: () -> Unit) {
    val fill = colour ?: MaterialTheme.colorScheme.surfaceVariant
    Box(
        Modifier
            .size(36.dp)
            .clip(CircleShape)
            .background(fill)
            .then(
                if (selected) {
                    Modifier.border(3.dp, MaterialTheme.colorScheme.onSurface, CircleShape)
                } else if (colour == null) {
                    Modifier.border(1.dp, MaterialTheme.colorScheme.outline, CircleShape)
                } else {
                    Modifier
                },
            )
            .clickable(onClick = onClick)
            .semantics { contentDescription = if (colour == null) "No colour" else "Colour" },
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
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
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
