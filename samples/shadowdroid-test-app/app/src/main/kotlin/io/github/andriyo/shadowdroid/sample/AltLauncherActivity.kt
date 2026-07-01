package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import kotlin.math.roundToInt

class AltLauncherActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply {
            id = R.id.alt_root
            orientation = LinearLayout.VERTICAL
            setPadding(18.dp, 18.dp, 18.dp, 18.dp)
        }
        root.addView(TextView(this).apply {
            text = "Alternate launcher activity"
            textSize = 22f
            contentDescription = "Alternate launcher activity message"
        })
        root.addView(Button(this).apply {
            id = R.id.alt_open_main_button
            text = "Open main test screen"
            isAllCaps = false
            contentDescription = "Open main test screen button"
            setOnClickListener {
                startActivity(Intent(this@AltLauncherActivity, MainActivity::class.java))
            }
        })
        setContentView(root)
    }

    private val Int.dp: Int
        get() = (this * resources.displayMetrics.density).roundToInt()
}

