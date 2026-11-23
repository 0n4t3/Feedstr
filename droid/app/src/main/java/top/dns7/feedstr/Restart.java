package top.dns7.feedstr;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.os.Process;
import android.os.SystemClock;

import java.io.File;

/**
 * Starts Feedstr again in a new process once the one from before has ended,
 * for when it's opened while that is still ending (see MainActivity.endProcess).
 * Runs in a process of its own, so it outlives the one it waits for.
 */
public final class Restart extends Activity {
    private static final String PID = "pid";
    /** Longer than MainActivity waits for changes to be uploaded. */
    private static final long WAIT_MS = 12000;

    /** Starts Feedstr again once the process of {@code from} has ended. */
    static void start(Activity from) {
        from.startActivity(new Intent(from, Restart.class).putExtra(PID, Process.myPid()));
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        int pid = getIntent().getIntExtra(PID, 0);
        new Thread(() -> {
            if (pid > 0 && pid != Process.myPid()) {
                File process = new File("/proc/" + pid);
                long until = SystemClock.uptimeMillis() + WAIT_MS;
                while (process.exists() && SystemClock.uptimeMillis() < until) {
                    SystemClock.sleep(50);
                }
                if (process.exists()) {
                    Process.killProcess(pid);
                }
                // For Android to notice it's gone, so the app starts in a new process.
                SystemClock.sleep(200);
            }
            runOnUiThread(() -> {
                startActivity(new Intent(this, MainActivity.class)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TASK));
                finish();
            });
        }, "feedstr-restart").start();
    }
}
