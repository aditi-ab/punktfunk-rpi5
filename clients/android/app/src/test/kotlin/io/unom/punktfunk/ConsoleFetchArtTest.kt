package io.unom.punktfunk

import io.unom.punktfunk.console.fetchArt
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** A malformed cover URL costs that cover, never the console's art thread. */
class ConsoleFetchArtTest {
    private val poster = byteArrayOf(1, 2, 3)

    // Answers every request itself, so the walk runs without a network.
    private val client = OkHttpClient.Builder().addInterceptor { chain ->
        Response.Builder().request(chain.request()).protocol(Protocol.HTTP_1_1).code(200).message("OK")
            .body(poster.toResponseBody()).build()
    }.build()

    @Test
    fun malformedUrlFallsThroughToTheNextCandidate() {
        assertArrayEquals(poster, fetchArt(listOf("not a url", "https://art.example/p.png"), client, offline = false))
    }

    @Test
    fun onlyMalformedUrlsIsNoCover() {
        assertNull(fetchArt(listOf("not a url", "ftp://art.example/p.png"), client, offline = true))
    }
}
