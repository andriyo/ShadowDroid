package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.content.Intent
import android.content.res.Configuration
import android.database.sqlite.SQLiteDatabase
import android.graphics.Color
import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import java.util.UUID

/** Seeded tool-contract fixture; never included in claims of agent benchmark uplift. */
open class VerificationFixtureActivity : Activity() {
    protected open val broken = false
    private lateinit var name: EditText
    private lateinit var saved: TextView

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        val dark = !broken && resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK == Configuration.UI_MODE_NIGHT_YES
        val foreground = if (dark) Color.WHITE else Color.BLACK
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(40, 120, 40, 40)
            setBackgroundColor(if (dark) Color.BLACK else Color.WHITE)
        }
        fun label(id: Int, value: String) = TextView(this).apply {
            this.id = id
            text = value
            textSize = 20f
            setTextColor(foreground)
            root.addView(this)
        }
        label(R.id.verification_title, "Verification form")
        label(R.id.verification_instance, UUID.randomUUID().toString())
        label(R.id.verification_theme, if (dark) "Dark" else "Light")
        name = EditText(this).apply {
            id = R.id.verification_name
            hint = "Name"
            setTextColor(foreground)
            setHintTextColor(foreground)
            isSaveEnabled = false // State preservation is intentionally explicit in this pair.
            if (!broken) setText(state?.getString("name") ?: "")
        }
        root.addView(name)
        saved = label(R.id.verification_saved, "Not saved")
        root.addView(Button(this).apply {
            id = R.id.verification_save
            text = "Save record"
            setOnClickListener {
                if (!broken) database().use { db ->
                    db.execSQL("INSERT INTO records(value) VALUES(?)", arrayOf(name.text.toString()))
                }
                // The defective variant claims success in UI without writing a row.
                saved.text = "Saved: ${name.text}"
            }
        })
        root.addView(Button(this).apply {
            id = R.id.verification_next
            text = "Secondary route"
            setOnClickListener {
                if (broken) throw IllegalStateException("Seeded missing secondary-route binding")
                startActivity(Intent(this@VerificationFixtureActivity, VerificationDetailActivity::class.java))
            }
        })
        setContentView(root)
        database().close()
    }

    private fun database(): SQLiteDatabase = openOrCreateDatabase("verification.db", MODE_PRIVATE, null).apply {
        enableWriteAheadLogging()
        execSQL("CREATE TABLE IF NOT EXISTS records(id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
    }

    override fun onSaveInstanceState(outState: Bundle) {
        if (!broken) outState.putString("name", name.text.toString())
        super.onSaveInstanceState(outState)
    }
}

class BrokenVerificationFixtureActivity : VerificationFixtureActivity() {
    override val broken = true
}

class VerificationDetailActivity : Activity() {
    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        setContentView(TextView(this).apply {
            id = R.id.verification_detail
            text = "Secondary route ready"
            textSize = 24f
            setPadding(40, 150, 40, 40)
        })
    }
}
