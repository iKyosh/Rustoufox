// Opens a page with the Playwright inspector, for `rustoufox test`.
//
// Usage: node inspect.js <playwright package dir> <base64 JSON {launch, url, contextOptions}>
const path = require('path')

const driverPackage = process.argv[2]
const { launch, url, contextOptions } = JSON.parse(Buffer.from(process.argv[3], 'base64').toString())
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
