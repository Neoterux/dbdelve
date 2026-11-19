package dev.dbdelve.android

import android.app.ForegroundServiceStartNotAllowedException
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch

/**
 * Holds the process at foreground priority while a statement runs, and
 * nothing else: the session stays in the view model. Some Android builds
 * reclaim background apps within minutes, which would drop a long statement
 * the moment the user switched away to check something.
 *
 * It stops itself, after entering the foreground, once [running] clears.
 * Stopping it from outside could land before `startForeground`, which
 * Android punishes by crashing the app, and most statements finish in
 * milliseconds. The app never asks for the notification permission, so on
 * Android 15 the notification stays out of the shade and nothing flickers.
 */
class QueryService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)

    override fun onBind(intent: Intent?) = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, "Running statements", NotificationManager.IMPORTANCE_LOW),
        )
        // The connection's name only: never the statement, which may hold
        // anything the user typed.
        val notification = Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle("Running a statement")
            .setContentText(running.value.orEmpty())
            .setOngoing(true)
            .build()
        startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        scope.launch {
            running.first { it == null }
            stopSelf()
        }
        return START_NOT_STICKY
    }

    /** Android 15 caps a data-sync service; the statement goes on without it. */
    override fun onTimeout(startId: Int, fgsType: Int) {
        stopSelf()
    }

    override fun onDestroy() {
        scope.cancel()
    }

    companion object {
        private const val CHANNEL = "statements"
        private const val NOTIFICATION_ID = 1

        /** The connection a statement is running on, or null when none is. */
        private val running = MutableStateFlow<String?>(null)

        fun keepAlive(context: Context, connection: String?) {
            running.value = connection
            if (connection == null) return
            try {
                context.startForegroundService(Intent(context, QueryService::class.java))
            } catch (refused: ForegroundServiceStartNotAllowedException) {
                // Run was tapped, so the app is in front; this is the rare
                // run started as the app left the screen. It goes unprotected.
            }
        }
    }
}
