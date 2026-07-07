package io.unom.punktfunk

import android.content.res.Configuration
import androidx.activity.compose.BackHandler
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedTextField
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

// The gamepad-driven "Add Host" screen — the Android mirror of the Apple client's GamepadAddHostView
// + GamepadKeyboard: three field rows (name / address / port) plus an Add action, navigated with the
// vertical focus list; A on a field opens the on-screen keyboard so a host can be registered end to
// end from the couch. One GamepadNavEffect2D owns BOTH modes (list vs keyboard) so they never fight
// over the shared input probes. B peels one layer: close the keyboard, then cancel the screen.

// Keyboard grid: digits, qwerty letters, hostname/address punctuation, then space / delete / done.
private val KB_CHAR_ROWS = listOf("1234567890", "qwertyuiop", "asdfghjkl-", "zxcvbnm._:")
private const val KB_ACTIONS_ROW = 4 // index of the [space, delete, done] row
private const val KB_ROWS = 5

private class Field(val id: String, val label: String, val value: String, val placeholder: String)

@Composable
fun GamepadAddHostScreen(
    onAdd: (name: String, address: String, port: Int) -> Unit,
    onDismiss: () -> Unit,
    // Non-null → EDIT mode: fields seed from this host, a MAC row is added, and the action SAVES the
    // edited record via [onSave] instead of connecting. [suggestedMacs] prefills a not-yet-learned MAC.
    editHost: KnownHost? = null,
    suggestedMacs: List<String> = emptyList(),
    onSave: ((KnownHost) -> Unit)? = null,
) {
    val context = LocalContext.current
    val isTv = remember { isTvDevice(context) }
    val isEdit = editHost != null
    val title = if (isEdit) "Edit Host" else "Add Host"
    val actionLabel = if (isEdit) "Save" else "Add Host"
    var name by remember { mutableStateOf(editHost?.name ?: "") }
    var address by remember { mutableStateOf(editHost?.address ?: "") }
    var port by remember { mutableStateOf(editHost?.port?.toString() ?: "9777") }
    var mac by remember { mutableStateOf(editHost?.mac?.ifEmpty { suggestedMacs }?.joinToString(", ") ?: "") }
    val canAdd = address.isNotBlank() && (port.toIntOrNull() ?: 0) > 0
    fun commit() {
        if (isEdit && editHost != null && onSave != null) {
            onSave(
                editHost.copy(
                    name = name.trim().ifEmpty { editHost.address },
                    address = address.trim(),
                    port = port.toIntOrNull() ?: editHost.port,
                    mac = KnownHostStore.parseMacs(mac),
                ),
            )
        } else {
            onAdd(name.trim(), address.trim(), port.toIntOrNull() ?: 9777)
        }
    }

    // On a TV the OS provides a leanback on-screen keyboard for text fields, so use real (focusable)
    // text fields + the system IME there. Our controller keyboard is for a phone-with-controller,
    // where the phone's own soft keyboard needs a touch a pad can't provide.
    if (isTv) {
        TvAddHostForm(
            title = title, actionLabel = actionLabel,
            name = name, onName = { name = it },
            address = address, onAddress = { address = it },
            port = port, onPort = { port = it.filter(Char::isDigit).take(5) },
            mac = if (isEdit) mac else null, onMac = { mac = it },
            canAdd = canAdd,
            onAdd = { commit() },
            onDismiss = onDismiss,
        )
        return
    }

    var focus by remember { mutableIntStateOf(1) } // start on Address
    var editing by remember { mutableStateOf<String?>(null) } // field id being typed, or null
    var kbRow by remember { mutableIntStateOf(1) }
    var kbCol by remember { mutableIntStateOf(0) }
    val landscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE
    val hazeState = remember { HazeState() }

    val fields = buildList {
        add(Field("name", "Name", name, "Optional — e.g. Living Room"))
        add(Field("address", "Address", address, "IP or hostname"))
        add(Field("port", "Port", port, "9777"))
        if (isEdit) add(Field("mac", "Wake MAC", mac, "auto-filled when the host is seen"))
    }
    val actionIndex = fields.size // the Save/Add action sits just after the last field

    fun openKeyboard(id: String) { editing = id; kbRow = 1; kbCol = 0 }
    fun closeKeyboard() { editing = null }
    fun editField(id: String, transform: (String) -> String) {
        when (id) {
            "name" -> name = transform(name)
            "address" -> address = transform(address)
            "port" -> port = transform(port).take(5)
            "mac" -> mac = transform(mac)
        }
    }
    fun allowed(id: String, c: Char): Boolean = when (id) {
        "port" -> c.isDigit()
        "address" -> c != ' '
        else -> true
    }
    fun activateField() {
        if (focus == actionIndex) {
            if (canAdd) commit() else { focus = 1; openKeyboard("address") }
        } else {
            openKeyboard(fields[focus].id)
        }
    }
    fun pressKey() {
        val id = editing ?: return
        if (kbRow < KB_ACTIONS_ROW) {
            val c = KB_CHAR_ROWS[kbRow][kbCol.coerceIn(0, KB_CHAR_ROWS[kbRow].lastIndex)]
            if (allowed(id, c)) editField(id) { it + c }
        } else when (kbCol) {
            0 -> if (allowed(id, ' ')) editField(id) { "$it " }
            1 -> editField(id) { it.dropLast(1) }
            else -> closeKeyboard()
        }
    }

    BackHandler { if (editing != null) closeKeyboard() else onDismiss() }
    GamepadNavEffect2D(
        active = true,
        onDirection = { dir ->
            if (editing == null) {
                when (dir) {
                    NavDir.UP -> if (focus > 0) focus--
                    NavDir.DOWN -> if (focus < actionIndex) focus++
                    else -> {}
                }
            } else {
                when (dir) {
                    NavDir.UP -> if (kbRow > 0) { kbRow--; kbCol = kbCol.coerceIn(0, rowCols(kbRow) - 1) }
                    NavDir.DOWN -> if (kbRow < KB_ROWS - 1) { kbRow++; kbCol = kbCol.coerceIn(0, rowCols(kbRow) - 1) }
                    NavDir.LEFT -> if (kbCol > 0) kbCol--
                    NavDir.RIGHT -> if (kbCol < rowCols(kbRow) - 1) kbCol++
                }
            }
        },
        onActivate = { if (editing == null) activateField() else pressKey() },
        onTertiary = { if (editing != null) editField(editing!!) { it.dropLast(1) } },
        onSecondary = { if (editing != null) closeKeyboard() },
    )

    val onFieldClick: (Int) -> Unit = { i -> if (focus == i) activateField() else focus = i }
    val onAddClick: () -> Unit = { if (focus == actionIndex) activateField() else focus = actionIndex }
    // Tappable (touch escape hatch): the legend doubles as buttons when there's no working controller.
    val typeHints = listOf(
        PadGlyph.hint('A', "Type") { pressKey() },
        PadGlyph.hint('X', "Delete") { editing?.let { id -> editField(id) { it.dropLast(1) } } },
        PadGlyph.hint('B', "Done") { closeKeyboard() },
    )
    val sideBySide = landscape && editing != null

    Box(Modifier.fillMaxSize()) {
        Box(Modifier.fillMaxSize().hazeSource(hazeState)) {
            GamepadFormBackground(Modifier.fillMaxSize())

            if (sideBySide) {
                // Landscape + typing: fields and keyboard SIDE BY SIDE so the field being edited stays
                // visible (stacked, the keyboard covered the whole short screen). The legend is NOT put
                // under the keyboard here — it floats at the same fixed bottom-left spot as everywhere.
                Row(
                    Modifier.fillMaxSize().systemBarsPadding().padding(start = ConsoleEdgeInset, end = 20.dp, top = 8.dp, bottom = 8.dp),
                    horizontalArrangement = Arrangement.spacedBy(18.dp),
                ) {
                    Column(
                        Modifier.weight(1f).fillMaxHeight().verticalScroll(rememberScrollState()),
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        ConsoleHeader(title, horizontalInset = false)
                        fields.forEachIndexed { i, f -> FieldRow(f, focused = false, editing = editing == f.id) { onFieldClick(i) } }
                        AddActionRow(actionLabel, enabled = canAdd, focused = false) { onAddClick() }
                        Spacer(Modifier.height(64.dp)) // clear the floating legend at bottom-left
                    }
                    Column(
                        Modifier.weight(1.15f).fillMaxHeight().verticalScroll(rememberScrollState()),
                        verticalArrangement = Arrangement.spacedBy(10.dp),
                    ) {
                        KeyboardGrid(kbRow, kbCol, compact = true) { r, c -> kbRow = r; kbCol = c; pressKey() }
                    }
                }
            } else {
                // Portrait (or landscape not typing): the FORM SCROLLS so the Add button is never
                // compressed by the keyboard; the keyboard sits below it; the legend floats (fixed).
                Column(Modifier.fillMaxSize().systemBarsPadding().padding(horizontal = ConsoleEdgeInset)) {
                    Column(
                        Modifier.weight(1f).fillMaxWidth().verticalScroll(rememberScrollState()),
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        ConsoleHeader(title, horizontalInset = false)
                        if (editing == null && !landscape) {
                            Text(
                                "Hosts on this network appear automatically — add one by address for everything else.",
                                style = MaterialTheme.typography.bodyMedium,
                                color = Color.White.copy(alpha = 0.55f),
                                modifier = Modifier.widthIn(max = 520.dp).padding(bottom = 8.dp),
                            )
                        }
                        fields.forEachIndexed { i, f -> FieldRow(f, focused = focus == i && editing == null, editing = editing == f.id) { onFieldClick(i) } }
                        AddActionRow(actionLabel, enabled = canAdd, focused = focus == actionIndex && editing == null) { onAddClick() }
                        Spacer(Modifier.height(72.dp)) // last field clears the floating legend when scrolled
                    }
                    if (editing != null) {
                        Spacer(Modifier.height(8.dp))
                        // The keyboard fills to the bottom; its bottom frame is padded so the fixed
                        // legend sits OVER that frame (bottom-left corner) rather than in a gap below.
                        KeyboardGrid(kbRow, kbCol, compact = false, bottomInset = 52.dp) { r, c -> kbRow = r; kbCol = c; pressKey() }
                    }
                }
            }
        }

        // Floating legend — ALWAYS at the same fixed bottom-start spot (portrait or landscape, keyboard
        // open or not), so opening the keyboard never relocates it below the keys. Backdrop-blurred.
        Box(
            Modifier.align(Alignment.BottomStart)
                .then(if (landscape) Modifier else Modifier.systemBarsPadding())
                .padding(ConsoleLegendInset),
        ) {
            GamepadHintBar(
                if (editing != null) {
                    typeHints
                } else {
                    listOf(
                        PadGlyph.hint('A', "Select") { activateField() },
                        PadGlyph.hint('B', "Cancel", onClick = onDismiss),
                    )
                },
                hazeState = hazeState,
            )
        }
    }
}

/**
 * Add-Host on a TV: real focusable text fields + the system (leanback) IME, driven by the OS. No
 * custom keyboard or input probes — the native focus engine moves between fields and the Add button,
 * and focusing a field pops the OS keyboard. B backs out.
 */
@Composable
private fun TvAddHostForm(
    title: String,
    actionLabel: String,
    name: String,
    onName: (String) -> Unit,
    address: String,
    onAddress: (String) -> Unit,
    port: String,
    onPort: (String) -> Unit,
    mac: String?, // non-null only in edit mode
    onMac: (String) -> Unit,
    canAdd: Boolean,
    onAdd: () -> Unit,
    onDismiss: () -> Unit,
) {
    BackHandler(onBack = onDismiss)
    val firstFocus = remember { FocusRequester() }
    Box(Modifier.fillMaxSize()) {
        GamepadFormBackground(Modifier.fillMaxSize())
        Column(
            Modifier
                .fillMaxSize()
                .systemBarsPadding()
                .padding(horizontal = 56.dp, vertical = 36.dp)
                .widthIn(max = 720.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Text(title, style = MaterialTheme.typography.headlineMedium, fontWeight = FontWeight.Bold, color = Color.White)
            Text(
                "Hosts on this network appear automatically — add one by address for everything else.",
                style = MaterialTheme.typography.bodyMedium,
                color = Color.White.copy(alpha = 0.55f),
            )
            OutlinedTextField(
                value = name, onValueChange = onName, singleLine = true,
                label = { Text("Name (optional)") },
                modifier = Modifier.fillMaxWidth().focusRequester(firstFocus),
            )
            OutlinedTextField(
                value = address, onValueChange = onAddress, singleLine = true,
                label = { Text("Address") },
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = port, onValueChange = onPort, singleLine = true,
                label = { Text("Port") },
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                modifier = Modifier.fillMaxWidth(),
            )
            if (mac != null) {
                OutlinedTextField(
                    value = mac, onValueChange = onMac, singleLine = true,
                    label = { Text("Wake-on-LAN MAC") },
                    placeholder = { Text("auto-filled when the host is seen") },
                    modifier = Modifier.fillMaxWidth(),
                )
            }
            Button(onClick = onAdd, enabled = canAdd, modifier = Modifier.fillMaxWidth()) {
                Text(actionLabel)
            }
        }
    }
    LaunchedEffect(Unit) { runCatching { firstFocus.requestFocus() } }
}

private fun rowCols(row: Int): Int = if (row < KB_ACTIONS_ROW) KB_CHAR_ROWS[row].length else 3

@Composable
private fun FieldRow(f: Field, focused: Boolean, editing: Boolean, onClick: () -> Unit) {
    val visuals = animateConsoleFocus(active = focused || editing, editing = editing)
    val shape = RoundedCornerShape(14.dp)
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .graphicsLayer { scaleX = visuals.scale; scaleY = visuals.scale }
            .clip(shape)
            .background(visuals.background)
            .border(1.dp, visuals.border, shape)
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null, onClick = onClick)
            .padding(horizontal = 16.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(f.label, style = MaterialTheme.typography.bodyLarge, fontWeight = FontWeight.SemiBold, color = Color.White)
        Spacer(Modifier.weight(1f))
        Text(
            f.value.ifEmpty { f.placeholder },
            style = MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace),
            color = if (f.value.isEmpty()) Color.White.copy(alpha = 0.35f) else Color.White,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        if (editing) Text(" |", color = Color(0xFF8678F5))
    }
}

@Composable
private fun AddActionRow(label: String, enabled: Boolean, focused: Boolean, onClick: () -> Unit) {
    val visuals = animateConsoleFocus(active = focused)
    val shape = RoundedCornerShape(14.dp)
    val labelColor by animateColorAsState(
        if (enabled) Color(0xFF8678F5) else Color.White.copy(alpha = 0.35f),
        tween(160),
        label = "addLabel",
    )
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .graphicsLayer { scaleX = visuals.scale; scaleY = visuals.scale }
            .clip(shape)
            .background(visuals.background)
            .border(1.dp, visuals.border, shape)
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null, onClick = onClick)
            .padding(vertical = 14.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            label,
            style = MaterialTheme.typography.bodyLarge,
            fontWeight = FontWeight.Bold,
            color = labelColor,
        )
    }
}

@Composable
private fun KeyboardGrid(
    cursorRow: Int,
    cursorCol: Int,
    compact: Boolean,
    bottomInset: Dp = 0.dp, // empty frame at the bottom of the glass for the floating legend to sit over
    onKey: (Int, Int) -> Unit,
) {
    val shape = RoundedCornerShape(20.dp)
    val gap = if (compact) 5.dp else 7.dp
    Column(
        Modifier
            .fillMaxWidth()
            .widthIn(max = 640.dp)
            .clip(shape)
            .background(Color(0x1FFFFFFF))
            .border(1.dp, Color.White.copy(alpha = 0.12f), shape)
            .padding(start = 12.dp, end = 12.dp, top = if (compact) 8.dp else 12.dp, bottom = 12.dp + bottomInset),
        verticalArrangement = Arrangement.spacedBy(gap),
    ) {
        KB_CHAR_ROWS.forEachIndexed { r, chars ->
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(gap)) {
                chars.forEachIndexed { c, ch ->
                    Keycap(ch.toString(), focused = cursorRow == r && cursorCol == c, compact = compact, modifier = Modifier.weight(1f)) { onKey(r, c) }
                }
            }
        }
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(gap)) {
            Keycap("space", focused = cursorRow == KB_ACTIONS_ROW && cursorCol == 0, compact = compact, modifier = Modifier.weight(2f)) { onKey(KB_ACTIONS_ROW, 0) }
            Keycap("⌫", focused = cursorRow == KB_ACTIONS_ROW && cursorCol == 1, compact = compact, modifier = Modifier.weight(1f)) { onKey(KB_ACTIONS_ROW, 1) }
            Keycap("Done", focused = cursorRow == KB_ACTIONS_ROW && cursorCol == 2, compact = compact, modifier = Modifier.weight(1.5f)) { onKey(KB_ACTIONS_ROW, 2) }
        }
    }
}

@Composable
private fun Keycap(label: String, focused: Boolean, compact: Boolean, modifier: Modifier = Modifier, onClick: () -> Unit) {
    // Fast tweens: the keyboard cursor hops many keys per second under hold-to-repeat, so the
    // trailing key must have faded before the cursor is two keys away — quick, but no longer a snap.
    val bg by animateColorAsState(
        if (focused) Color(0xFF8678F5) else Color(0x14FFFFFF),
        tween(90),
        label = "keyBg",
    )
    val fg by animateColorAsState(if (focused) Color.Black else Color.White, tween(90), label = "keyFg")
    Box(
        modifier = modifier
            .height(if (compact) 34.dp else 44.dp)
            .clip(RoundedCornerShape(9.dp))
            .background(bg)
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            label,
            style = MaterialTheme.typography.bodyLarge,
            fontWeight = FontWeight.Medium,
            color = fg,
            textAlign = TextAlign.Center,
        )
    }
}
