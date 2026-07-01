package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.os.Bundle
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import kotlin.math.roundToInt

class DetailActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply {
            id = R.id.detail_root
            orientation = LinearLayout.VERTICAL
            setPadding(18.dp, 18.dp, 18.dp, 18.dp)
        }
        root.addView(TextView(this).apply {
            id = R.id.detail_message
            text = "Detail activity opened from ${intent.getStringExtra("source") ?: "unknown"}"
            textSize = 20f
            contentDescription = "Detail activity message"
        })
        root.addView(Button(this).apply {
            id = R.id.detail_finish_button
            text = "Finish detail"
            isAllCaps = false
            contentDescription = "Finish detail activity button"
            setOnClickListener { finish() }
        })
        setContentView(root)
    }

    private val Int.dp: Int
        get() = (this * resources.displayMetrics.density).roundToInt()
}

