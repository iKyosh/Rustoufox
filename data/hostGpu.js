// Reads the (vendor, renderer) the host's Firefox reports, with no identity
// applied, through playwright-core. Used by rustoufox when Node.js and a
// Playwright package are available: without them the host GPU is unknown and
// the launcher treats the host as rendering in software.
//
// Usage: node hostGpu.js <playwright package dir> <base64 json options>
const path = require('path')

const driverPackage = process.argv[2]
const options = JSON.parse(Buffer.from(process.argv[3], 'base64').toString())

let playwright
try {
    playwright = require(path.join(driverPackage, 'index.js'))
} catch (error) {
    console.error(`Error loading Playwright from ${driverPackage}:`, error.message)
    process.exit(1)
}

const READ_GPU = `() => {
    const gl = document.createElement('canvas').getContext('webgl');
    const info = gl && gl.getExtension('WEBGL_debug_renderer_info');
    return info
        ? [gl.getParameter(info.UNMASKED_VENDOR_WEBGL), gl.getParameter(info.UNMASKED_RENDERER_WEBGL)]
        : null;
}`

async function main() {
    const browser = await playwright.firefox.launch({
        executablePath: options.executablePath,
        headless: options.headless,
        env: options.env,
        firefoxUserPrefs: { 'webgl.force-enabled': true },
    })
    try {
        const page = await browser.newPage()
        const gpu = await page.evaluate(READ_GPU)
        process.stdout.write(JSON.stringify(gpu) + '\n')
    } finally {
        await browser.close()
    }
}

main().catch((error) => {
    console.error('Error reading the host GPU:', error.message)
    process.exit(1)
})
