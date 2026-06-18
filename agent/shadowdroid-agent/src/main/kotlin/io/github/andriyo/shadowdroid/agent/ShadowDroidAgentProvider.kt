package io.github.andriyo.shadowdroid.agent

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.net.Uri
import android.util.Log

/**
 * Zero-config entry point. AGP merges this provider into the host app's
 * manifest, so the framework instantiates it and calls [onCreate] before
 * `Application.onCreate()` — no app code required. It only ships in builds that
 * pull the AAR via `debugImplementation`.
 *
 * This is not a real content provider; every data method is a no-op. Its sole
 * job is to bootstrap [ShadowDroidAgent], guarded so a debug tool can never
 * destabilise the host app.
 */
class ShadowDroidAgentProvider : ContentProvider() {

    override fun onCreate(): Boolean {
        val ctx = context?.applicationContext ?: return true
        try {
            ShadowDroidAgent.start(ctx)
        } catch (t: Throwable) {
            Log.w(ShadowDroidAgent.TAG, "agent failed to start", t)
        }
        return true
    }

    override fun query(
        uri: Uri,
        projection: Array<out String>?,
        selection: String?,
        selectionArgs: Array<out String>?,
        sortOrder: String?,
    ): Cursor? = null

    override fun getType(uri: Uri): String? = null

    override fun insert(uri: Uri, values: ContentValues?): Uri? = null

    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int = 0

    override fun update(
        uri: Uri,
        values: ContentValues?,
        selection: String?,
        selectionArgs: Array<out String>?,
    ): Int = 0
}
