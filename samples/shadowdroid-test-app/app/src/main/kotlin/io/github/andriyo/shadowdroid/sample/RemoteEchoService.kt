package io.github.andriyo.shadowdroid.sample

import android.app.Service
import android.content.Intent
import android.os.IBinder
import android.util.Log

// Runs in the ":remote" process so the debug app exposes two debuggable
// processes — exercises debugger attach package disambiguation.
class RemoteEchoService : Service() {
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Log.i(TAG, "RemoteEchoService started in :remote process")
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private companion object {
        const val TAG = "ShadowDroidSample"
    }
}
