package top.dns7.feedstr;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

/**
 * Keeps the app running while it syncs in the background. Android freezes an
 * app soon after it leaves the screen (say, to read an article in the
 * browser), which made syncs under way fail. A foreground service prevents
 * that. It does no work itself: the Rust app syncs as before and stops the
 * service once it's done (see keep_running in feedstr-droid).
 *
 * The notification only appears for syncs that take a while: Android waits a
 * few seconds before showing it, and a service that stops sooner shows none.
 */
public final class SyncService extends Service {
    private static final String TAG = "feedstr";
    private static final String CHANNEL = "sync";
    private static final int NOTIFICATION = 1;

    /**
     * Whether the app still needs the service; it stops itself once this is
     * false. Like everything here, only used on the main thread, so starting
     * and stopping happen in the order they were asked for.
     */
    private static boolean wanted;
    /** The running service, if any (main thread only). */
    private static SyncService running;

    /** The latest start, so stopping doesn't cut off one that's still on its way. */
    private int lastStart;

    /** Starts the service (or keeps it). Only works while the app is on screen or just leaving it. Main thread. */
    static void start(Context context) {
        wanted = true;
        try {
            context.startForegroundService(new Intent(context, SyncService.class));
        } catch (RuntimeException e) {
            // Android 12 and later refuse it from the background (say, for a
            // scheduled sync): the sync goes ahead without it.
            wanted = false;
            Log.w(TAG, "couldn't keep the app running in the background: " + e);
        }
    }

    /** Lets the service stop. Main thread. */
    static void stop() {
        wanted = false;
        // If it hasn't started yet, it stops as soon as it does (see onStartCommand).
        if (running != null) {
            running.finish();
        }
    }

    @Override
    public void onCreate() {
        super.onCreate();
        running = this;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        lastStart = startId;
        // Android requires this soon after starting, even if the service is no longer wanted.
        Notification notification = notification();
        try {
            if (Build.VERSION.SDK_INT >= 29) {
                startForeground(NOTIFICATION, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC);
            } else {
                startForeground(NOTIFICATION, notification);
            }
        } catch (RuntimeException e) {
            // Say, Android 15's daily limit for data sync services was used up.
            Log.w(TAG, "couldn't keep the app running in the background: " + e);
            stopSelf();
            return START_NOT_STICKY;
        }
        if (!wanted) {
            finish();
        }
        return START_NOT_STICKY;
    }

    /** Android 15 and later limit how long a data sync service runs a day. */
    @Override
    public void onTimeout(int startId, int fgsType) {
        // It must stop now, or Android stops the app.
        stopForeground(STOP_FOREGROUND_REMOVE);
        stopSelf();
    }

    @Override
    public void onDestroy() {
        if (running == this) {
            running = null;
        }
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    /**
     * Stops, unless the app asked for it again since the latest start it got:
     * Android then expects that start to reach onStartCommand (and
     * startForeground), and would stop the app if the service were gone.
     */
    private void finish() {
        if (stopSelfResult(lastStart)) {
            stopForeground(STOP_FOREGROUND_REMOVE);
        }
    }

    private Notification notification() {
        NotificationManager manager = getSystemService(NotificationManager.class);
        NotificationChannel channel =
                new NotificationChannel(CHANNEL, "Syncing", NotificationManager.IMPORTANCE_LOW);
        channel.setDescription("Shown while Feedstr finishes syncing in the background");
        channel.setShowBadge(false);
        manager.createNotificationChannel(channel);
        Intent open = new Intent(this, MainActivity.class).setFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        PendingIntent tap = PendingIntent.getActivity(this, 0, open, PendingIntent.FLAG_IMMUTABLE);
        return new Notification.Builder(this, CHANNEL)
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setContentTitle("Syncing…")
                .setContentText("Feedstr is finishing a sync")
                .setProgress(0, 0, true)
                .setContentIntent(tap)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setCategory(Notification.CATEGORY_PROGRESS)
                .build();
    }
}
