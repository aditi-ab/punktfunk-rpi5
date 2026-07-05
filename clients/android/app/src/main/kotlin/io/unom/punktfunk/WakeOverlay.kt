package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Bedtime
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * The "Waking <host>…" modal shown while [WakeController] brings a sleeping host back — a spinner + a
 * live elapsed counter, escalating to a retry/cancel prompt on timeout. The Android mirror of the
 * Apple client's `WakeOverlay`. Rendered over BOTH the touch grid and the console home; it swallows
 * input to the screen behind it, and in console mode the pad drives it (B cancels, A retries once
 * timed out) while the touch buttons work for a pointer.
 */
@Composable
fun WakeOverlay(waker: WakeController, gamepadUi: Boolean) {
    val w = waker.waking ?: return

    BackHandler { waker.cancel() } // system Back / pad B (remapped) cancels the wait
    if (gamepadUi) {
        // A retries once timed out; B falls through to the BackHandler above.
        GamepadNavEffect2D(
            active = true,
            onDirection = {},
            onActivate = { if (w.timedOut) waker.retry() },
        )
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(Color.Black.copy(alpha = 0.6f))
            // Swallow taps so the home behind can't be touched while waking.
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {},
        contentAlignment = Alignment.Center,
    ) {
        Column(
            Modifier
                .padding(40.dp)
                .widthIn(max = 380.dp)
                .clip(RoundedCornerShape(22.dp))
                .background(Color(0xF01A1730))
                .border(1.dp, Color.White.copy(alpha = 0.12f), RoundedCornerShape(22.dp))
                .padding(28.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            if (w.timedOut) {
                Icon(
                    Icons.Filled.Bedtime,
                    contentDescription = null,
                    tint = Color.White.copy(alpha = 0.85f),
                    modifier = Modifier.size(34.dp),
                )
                Text(
                    "${w.hostName} didn't wake",
                    color = Color.White,
                    fontWeight = FontWeight.Bold,
                    fontSize = 19.sp,
                    textAlign = TextAlign.Center,
                )
                Text(
                    "It may still be booting, or it's powered off / off this network.",
                    color = Color.White.copy(alpha = 0.6f),
                    fontSize = 13.sp,
                    textAlign = TextAlign.Center,
                )
                Row(
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                    modifier = Modifier.padding(top = 6.dp),
                ) {
                    OutlinedButton(onClick = { waker.cancel() }) { Text("Cancel") }
                    Button(onClick = { waker.retry() }) { Text("Try Again") }
                }
            } else {
                CircularProgressIndicator(color = Color.White)
                Text(
                    "Waking ${w.hostName}…",
                    color = Color.White,
                    fontWeight = FontWeight.Bold,
                    fontSize = 19.sp,
                    textAlign = TextAlign.Center,
                )
                Text(
                    "Waiting for it to come online · ${w.seconds}s",
                    color = Color.White.copy(alpha = 0.6f),
                    fontSize = 13.sp,
                    fontFamily = FontFamily.Monospace,
                )
                OutlinedButton(onClick = { waker.cancel() }, modifier = Modifier.padding(top = 6.dp)) {
                    Text(if (w.connectsAfter) "Cancel" else "Stop Waiting")
                }
            }
        }
    }
}
