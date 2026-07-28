package io.unom.punktfunk.components

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.models.HostStatus

/** Left-aligned section header above each block of the connect screen. */
@Composable
fun SectionLabel(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.fillMaxWidth().padding(bottom = 8.dp),
    )
}

/**
 * One row of a host card's overflow menu. [startsSection] draws a divider above it, which is how
 * the profile actions ("Connect with: …", "Pin as card: …") stay legible next to the host actions
 * in one flat menu — Compose has no submenus, and the Windows client made the same call.
 */
data class HostMenuItem(
    val label: String,
    val startsSection: Boolean = false,
    val onClick: () -> Unit,
)

/**
 * A host as an Apple-style card: a colored letter-avatar, name + address, a trust pill, and (for
 * saved hosts) an overflow menu with Wake / Edit / Forget plus whatever [menuItems] adds. Tapping
 * the card connects.
 *
 * [profileLabel] names the settings profile this card connects with. On a host's own card that is
 * its default binding, drawn as a quiet chip — the card says what a tap will do. On a **pinned
 * card** ([profileProminent]) the host name is still the title, but the profile is the loud part,
 * because the pin exists to make that one combination a single tap.
 */
@Composable
fun HostCard(
    name: String,
    address: String,
    status: HostStatus,
    online: Boolean = false,
    enabled: Boolean,
    onConnect: () -> Unit,
    onForget: (() -> Unit)?,
    onEdit: (() -> Unit)? = null,
    onWake: (() -> Unit)? = null,
    profileLabel: String? = null,
    profileProminent: Boolean = false,
    accent: Color? = null,
    menuItems: List<HostMenuItem> = emptyList(),
) {
    // D-pad / controller focus highlight: a clickable card is focusable, but the default state
    // layer is too subtle on a TV across a room — draw a clear primary-colour border when focused.
    var focused by remember { mutableStateOf(false) }
    ElevatedCard(
        onClick = onConnect,
        enabled = enabled,
        modifier = Modifier
            .fillMaxWidth()
            .padding(4.dp)
            .onFocusChanged { focused = it.isFocused }
            .then(
                if (focused) {
                    Modifier.border(2.dp, MaterialTheme.colorScheme.primary, CardDefaults.elevatedShape)
                } else {
                    Modifier
                },
            ),
    ) {
        Box(modifier = Modifier.fillMaxWidth()) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(16.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                HostAvatar(name)
                Spacer(Modifier.height(12.dp))
                Text(
                    name,
                    style = MaterialTheme.typography.titleMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    textAlign = TextAlign.Center,
                )
                Text(
                    address,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    textAlign = TextAlign.Center,
                )
                if (profileLabel != null) {
                    Spacer(Modifier.height(8.dp))
                    ProfileChip(profileLabel, accent, prominent = profileProminent)
                }
                Spacer(Modifier.height(12.dp))
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    PresencePill(online)
                    StatusPill(status)
                }
            }

            if (onForget != null || onEdit != null || onWake != null || menuItems.isNotEmpty()) {
                var menu by remember { mutableStateOf(false) }
                Box(modifier = Modifier.align(Alignment.TopEnd)) {
                    IconButton(enabled = enabled, onClick = { menu = true }) {
                        Icon(
                            Icons.Filled.MoreVert,
                            contentDescription = "More",
                            modifier = Modifier.size(20.dp),
                            tint = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.6f)
                        )
                    }
                    DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                        if (onWake != null) {
                            DropdownMenuItem(
                                text = { Text("Wake host") },
                                onClick = {
                                    menu = false
                                    onWake()
                                },
                            )
                        }
                        if (onEdit != null) {
                            DropdownMenuItem(
                                text = { Text("Edit…") },
                                onClick = {
                                    menu = false
                                    onEdit()
                                },
                            )
                        }
                        if (onForget != null) {
                            DropdownMenuItem(
                                text = { Text("Forget") },
                                onClick = {
                                    menu = false
                                    onForget()
                                },
                            )
                        }
                        menuItems.forEach { item ->
                            if (item.startsSection) HorizontalDivider()
                            DropdownMenuItem(
                                text = { Text(item.label) },
                                onClick = {
                                    menu = false
                                    item.onClick()
                                },
                            )
                        }
                    }
                }
            }
        }
    }
}

/**
 * The profile a card connects with. Quiet on a bound host's own card (it is a note about what a tap
 * does); filled and tinted on a pinned card, where the profile IS the reason the card exists — the
 * accent field the schema reserves earns its keep here.
 */
@Composable
private fun ProfileChip(label: String, accent: Color?, prominent: Boolean) {
    val tint = accent ?: MaterialTheme.colorScheme.primary
    Row(
        modifier = Modifier
            .clip(RoundedCornerShape(50))
            .background(tint.copy(alpha = if (prominent) 0.24f else 0.12f))
            .padding(horizontal = 10.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(7.dp).clip(CircleShape).background(tint))
        Spacer(Modifier.width(6.dp))
        Text(
            label,
            style = if (prominent) {
                MaterialTheme.typography.labelLarge
            } else {
                MaterialTheme.typography.labelMedium
            },
            color = tint,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

/** A circular avatar with the host's first letter (Apple-contact style). */
@Composable
fun HostAvatar(name: String) {
    val letter = name.trim().firstOrNull()?.uppercaseChar()?.toString() ?: "?"
    Box(
        modifier = Modifier
            .size(44.dp)
            .clip(CircleShape)
            .background(MaterialTheme.colorScheme.primaryContainer),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            letter,
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onPrimaryContainer,
        )
    }
}

/**
 * A small dot + label for live presence: green Online when the host advertises on mDNS OR answers
 * the reachability probe (so a routed/VPN host that never advertises still reads Online), dimmed
 * Offline otherwise.
 */
@Composable
fun PresencePill(online: Boolean) {
    val color =
        if (online) MaterialTheme.colorScheme.primary
        else MaterialTheme.colorScheme.onSurfaceVariant
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(8.dp).clip(CircleShape).background(color))
        Spacer(Modifier.width(6.dp))
        Text(
            if (online) "Online" else "Offline",
            style = MaterialTheme.typography.labelMedium,
            color = color,
        )
    }
}

/** A small colored dot + label for the host's trust state. */
@Composable
fun StatusPill(status: HostStatus) {
    val color = when (status) {
        HostStatus.PAIRED -> MaterialTheme.colorScheme.primary
        HostStatus.PAIRING -> MaterialTheme.colorScheme.tertiary
        HostStatus.TOFU -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(8.dp).clip(CircleShape).background(color))
        Spacer(Modifier.width(6.dp))
        Text(status.label, style = MaterialTheme.typography.labelMedium, color = color)
    }
}

/** Shown when there are no saved or discovered hosts. */
@Composable
fun EmptyHostsState() {
    Column(
        modifier = Modifier.fillMaxWidth().padding(vertical = 56.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("No hosts yet", style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(8.dp))
        Text(
            "Hosts on your network show up here automatically.\nTap “Add host” to enter one by address.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
}
