package io.unom.punktfunk

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build

// The clipboard half of "Copy link" (design/client-deep-links.md §4/§5), shared by every surface
// that hands a `punktfunk://` URL out: a host card, a pinned card, and a library title. The URL
// each one builds is its own business; whether the platform TOOK it, and what to say about that,
// is the same answer three times over — and getting it wrong in one place is how a menu item ends
// up silently doing nothing on exactly one screen.

/** Put a `punktfunk://` URL on the clipboard. False = no clipboard service, or it refused. */
internal fun putLinkOnClipboard(context: Context, url: String): Boolean {
    val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
        ?: return false
    return runCatching {
        clipboard.setPrimaryClip(ClipData.newPlainText("Punktfunk link", url))
    }.isSuccess
}

/**
 * What to tell the user about a copy, or null for "say nothing".
 *
 * Android 13 draws its own clipboard confirmation, and stacking a second one on top of it is the
 * platform's own documented anti-pattern. Below it nothing visible happens at all unless we say
 * so — a silent menu item reads as a broken one.
 */
internal fun linkCopyMessage(copied: Boolean): String? = when {
    copied && Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU -> null
    copied -> "Link copied."
    else -> "Couldn't copy the link to the clipboard."
}
