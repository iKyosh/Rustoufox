// Opens a page with the Playwright inspector, for `rustoufox test`.
//
// Usage: node inspect.js <playwright package dir>
// stdin: JSON {launch, url, contextOptions}, then closed. Not an argument: the
// launch carries the whole environment, past Linux's 128 KiB per argument.
const fs = require('fs')
const path = require('path')

const driverPackage = process.argv[2]
const { launch, url, contextOptions } = JSON.parse(fs.readFileSync(0, 'utf8'))
const playwright = require(path.join(driverPackage, 'index.js'))

async function main() {
    const browser = await playwright.firefox.launch(launch)
    const context = await browser.newContext(contextOptions)
    const page = await context.newPage()
    if (url) await page.goto(url)
    await page.pause()
    await browser.close()
}

main().catch((error) => {
    console.error(error.message)
    process.exit(1)
})
