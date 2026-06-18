package io.unom.punktfunk.components

import androidx.compose.foundation.background
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
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ElevatedCard
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
 * A host as an Apple-style card: a colored letter-avatar, name + address, a trust pill, and (for
 * saved hosts) an overflow menu with Forget. Tapping the card connects.
 */
@Composable
fun HostCard(
    name: String,
    address: String,
    status: HostStatus,
    enabled: Boolean,
    onConnect: () -> Unit,
    onForget: (() -> Unit)?,
) {
    ElevatedCard(
        onClick = onConnect,
        enabled = enabled,
        modifier = Modifier
            .fillMaxWidth()
            .padding(4.dp),
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
                Spacer(Modifier.height(12.dp))
                StatusPill(status)
            }

            if (onForget != null) {
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
                        DropdownMenuItem(
                            text = { Text("Forget") },
                            onClick = {
                                menu = false
                                onForget()
                            },
                        )
                    }
                }
            }
        }
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
