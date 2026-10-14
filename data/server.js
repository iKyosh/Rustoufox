// Runs Playwright's launchServer for rustoufox and keeps it up until stdin closes.
//
// Usage: node server.js <playwright package dir>
// stdin: one line of base64 JSON launchServer() options, then kept open.
// stdout: one JSON line {"wsEndpoint": "..."} once the server is listening.
const path = require('path')

const driverPackage = process.argv[2]
let playwright
try {
    playwright = require(path.join(driverPackage, 'index.js'))
} catch (error) {
    console.error(`Error loading Playwright from ${driverPackage}:`, error.message)
    process.exit(1)
}

process.stdin.setEncoding('utf8')

const stdinClosed = new Promise((resolve) => {
    process.stdin.once('end', resolve)
    process.stdin.once('close', resolve)
    process.stdin.once('error', resolve)
})

function readOptions() {
    return new Promise((resolve, reject) => {
        let data = ''
        const onData = (chunk) => {
            data += chunk
            const newline = data.indexOf('\n')
            if (newline !== -1) {
                process.stdin.removeListener('data', onData)
                try {
                    resolve(JSON.parse(Buffer.from(data.slice(0, newline), 'base64').toString()))
                } catch (error) {
                    reject(error)
                }
            }
        }
        process.stdin.on('data', onData)
        process.stdin.resume()
    })
}

async function main() {
    const options = await readOptions()
    // The launcher closes stdin to stop the server; a signal does the same.
    options.handleSIGINT = false
    options.handleSIGTERM = false
    options.handleSIGHUP = false
    const server = await playwright.firefox.launchServer(options)
    for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP'])
        process.on(signal, () => process.stdin.destroy())
    process.stdout.write(JSON.stringify({ wsEndpoint: server.wsEndpoint() }) + '\n')
    await stdinClosed
    await server.close()
}

main().catch((error) => {
    console.error('Error launching server:', error.message)
    process.exit(1)
})
