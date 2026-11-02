package io.unom.punktfunk

import android.app.ActivityManager
import android.content.Context
import android.os.Build
import androidx.annotation.RequiresApi
import io.unom.punktfunk.kit.NativeBridge
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * Keeps this device's log across a crash. [install] opens `files/logs/client.log`, the native
 * log's copy on disk that [SendLogs] sends, then writes an uncaught exception there before the
 * process dies and logs how the previous run ended: Android's exit record, with the crashing
 * thread of a native crash or the main thread of an ANR.
 */
object CrashLog {
    private val installed = AtomicBoolean(false)

    /** Once per process, first thing in [MainActivity.onCreate]; later calls do nothing. */
    fun install(context: Context) {
        if (!installed.compareAndSet(false, true)) return
        NativeBridge.nativeOpenLogFile(File(context.filesDir, "logs").path)
        val next = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { thread, e ->
            runCatching {
                NativeBridge.nativeLogCrash("uncaught on ${thread.name}: ${e.stackTraceToString()}")
            }
            next?.uncaughtException(thread, e)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            thread(name = "pf-exit-log") { logPreviousExit(context) } // a binder call, a file read
        }
    }

    /** The newest exit record is the previous run's: the app runs in one process. */
    @RequiresApi(Build.VERSION_CODES.R)
    private fun logPreviousExit(context: Context) {
        val am = context.getSystemService(ActivityManager::class.java)
        val exit = runCatching { am.getHistoricalProcessExitReasons(context.packageName, 0, 1) }
            .getOrNull()?.firstOrNull() ?: return
        val trace = runCatching { exit.traceInputStream?.use { it.readBytes() } }.getOrNull()
        NativeBridge.nativeLogExit(exit.toString(), exit.reason, trace ?: ByteArray(0))
    }
}
