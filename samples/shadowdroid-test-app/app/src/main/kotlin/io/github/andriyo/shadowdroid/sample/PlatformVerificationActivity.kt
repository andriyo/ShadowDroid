package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.app.PictureInPictureParams
import android.appwidget.AppWidgetManager
import android.appwidget.AppWidgetProvider
import android.content.Context
import android.content.Intent
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Build
import android.os.Bundle
import android.util.Rational
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.RemoteViews
import android.widget.TextView

/** Real Android boundary fixtures with explicit seeded defects. */
class PlatformVerificationActivity : Activity() {
    lateinit var root: LinearLayout
    lateinit var value: EditText
    var media: MediaSession? = null
    private val broken get() = intent.getBooleanExtra("broken", false)

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(30, 120, 30, 30) }
        value = EditText(this).apply { id = R.id.platform_value; hint = "Outbound value" }
        root.addView(value)
        root.addView(Button(this).apply {
            id = R.id.platform_send
            text = "Dispatch intent"
            setOnClickListener {
                startActivity(Intent(this@PlatformVerificationActivity, VerificationIntentReceiver::class.java)
                    .putExtra("payload", if (broken) "hardcoded" else value.text.toString()))
            }
        })
        root.addView(Button(this).apply {
            id = R.id.platform_media
            text = "Create media session"
            setOnClickListener {
                media?.release()
                media = MediaSession(this@PlatformVerificationActivity, "verification-fixture").apply {
                    setPlaybackState(PlaybackState.Builder().setState(PlaybackState.STATE_PLAYING, 0, 1f).build())
                    isActive = true
                }
            }
        })
        root.addView(Button(this).apply {
            id = R.id.platform_pip
            text = "Enter Picture-in-Picture"
            setOnClickListener {
                if (!broken && Build.VERSION.SDK_INT >= 26) {
                    enterPictureInPictureMode(PictureInPictureParams.Builder().setAspectRatio(Rational(16, 9)).build())
                }
            }
        })
        setContentView(root)
    }

    override fun onStop() {
        if (!broken) { media?.release(); media = null }
        super.onStop()
    }

    override fun onDestroy() {
        // Tests retain the broken session until this final cleanup so there is no leak after a failed assertion.
        media?.release()
        media = null
        super.onDestroy()
    }
}

class VerificationIntentReceiver : Activity() {
    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        setContentView(TextView(this).apply { text = intent.getStringExtra("payload"); textSize = 24f })
    }
}

class VerificationWidgetProvider : AppWidgetProvider() {
    override fun onUpdate(context: Context, manager: AppWidgetManager, ids: IntArray) {
        val preferences = context.getSharedPreferences("verification-platform", Context.MODE_PRIVATE)
        val text = if (preferences.getBoolean("broken", false)) "initial" else preferences.getString("value", "initial")
        for (id in ids) {
            manager.updateAppWidget(id, RemoteViews(context.packageName, R.layout.verification_widget).apply {
                setTextViewText(R.id.verification_widget_text, text)
            })
        }
    }
}
