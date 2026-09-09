// Vendored from Justo-Tapiador/node-jhs2 v2.1.0 (MIT, same author).
//
// File: index.js @ tag v2.1.0 — byte-identical engine below the marker,
// with this provenance header prepended. The wallermax-server "Node
// sidecar" backend (see [templates] backend in wallermax.toml) runs this
// engine inside worker threads so templates get the ORIGINAL runtime:
// real Node require() behind the forbidden_modules banner, the exact
// node-jhs2 compiler and its 2.1.0 parity fixes (echo(raw()) sentinel,
// mtime hot reload).
//
// The sidecar service (jhs-sidecar.mjs) and the render worker
// (render-worker.mjs) call two engine methods that node-jhs2 documents as
// internal (`_compile`, `_executeTemplate`); this copy is vendored AND
// version-pinned on purpose, so that dependency is explicit and frozen
// here instead of silently breaking on an upstream release. When
// upgrading: re-vendor, re-run `node sidecar/jhs-sidecar.mjs --selftest`
// and the wallermax test suite (tests/sidecar.rs).
//
// ──────────────────────── vendored node-jhs2 ────────────────────────
/**
 * NODE-JHS2 — Dynamic Template Engine for Node.js
 *
 * Allows embedding JavaScript inside HTML files using <?jhs ... ?> delimiters.
 * Similar to PHP but powered by JavaScript, using .jhs template files.
 * Includes automatic HTML escaping (XSS protection) and sandboxed VM execution.
 *
 * @module node-jhs2
 * @license MIT
 */

const fs = require('fs');
const path = require('path');
const vm = require('vm');

class JSTemplateEngine {
  /**
   * Creates a new template engine instance.
   * @param {object} options - Configuration options
   * @param {string}   [options.viewsPath='./views']   - Directory for .jhs template files
   * @param {boolean}  [options.cache=true]            - Enable in-memory template caching
   * @param {string}   [options.openTag='<?jhs']       - Opening tag for code blocks
   * @param {string}   [options.closeTag='?>']         - Closing tag
   * @param {string}   [options.echoTag='<?=']         - Opening tag for output expressions
   * @param {string}   [options.encoding='utf-8']      - File encoding
   * @param {boolean}  [options.autoEscape=true]       - Auto HTML-escape all output
   * @param {string[]} [options.banned_require=[]]     - Extra modules to block inside templates
   */
  constructor(options = {}) {
    this.options = {
      viewsPath: options.viewsPath || './views',
      cache: options.cache !== false,        // Cache enabled by default
      openTag: options.openTag || '<?jhs',
      closeTag: options.closeTag || '?>',
      echoTag: options.echoTag || '<?=',
      encoding: options.encoding || 'utf-8',
      autoEscape: options.autoEscape !== false, // Auto-escape enabled by default
      banned_require: ['vm', 'jhs'],            // Always blocked
      scriptname: '',
      error: null
    };

    this.require_filter = this.require_filter.bind(this);

    // Merge extra banned_require entries from options
    if (options != undefined) {
      if (options.banned_require != undefined) {
        this.options.banned_require.push(...options.banned_require);
        this.options.banned_require = [...new Set(this.options.banned_require)];
      }
    }

    this.templateCache = new Map();
  }

  /**
   * Renders a .jhs template file with the given data.
   * @param {string} templatePath - Path to the .jhs template file (relative to viewsPath or absolute)
   * @param {object} [data={}]    - Data variables available inside the template
   * @returns {Promise<string>}   Rendered HTML string
   */
  async render(templatePath, data = {}) {
    const fullPath = path.isAbsolute(templatePath)
      ? templatePath
      : path.join(this.options.viewsPath, templatePath);

    // Return from cache if available and the file has not changed since
    // compilation (mtime revalidation: edit a template, save, render again —
    // no clearCache() or restart needed).
    if (this.options.cache && this.templateCache.has(fullPath)) {
      const cached = this.templateCache.get(fullPath);
      let unchanged = false;
      try {
        unchanged = fs.statSync(fullPath).mtimeMs === cached.mtime;
      } catch (e) { /* file vanished: fall through and let readFile report it */ }
      if (unchanged) {
        return await this._executeTemplate(cached.compiledFn, data, fullPath);
      }
      this.templateCache.delete(fullPath); // stale entry — recompile below
    }

    // Read and compile the template
    const templateContent = await fs.promises.readFile(fullPath, this.options.encoding);
    const compiledFn = this._compile(templateContent);

    // Store in cache if caching is enabled (with mtime for hot reload)
    if (this.options.cache) {
      let mtime = null;
      try { mtime = fs.statSync(fullPath).mtimeMs; } catch (e) {}
      this.templateCache.set(fullPath, { compiledFn, mtime });
    }

    return await this._executeTemplate(compiledFn, data, fullPath);
  }

  /**
   * Renders a template from a raw string (no file read).
   * @param {string} template  - Template string
   * @param {object} [data={}] - Data variables available inside the template
   * @returns {Promise<string>} Rendered HTML string
   */
  async renderString(template, data = {}) {
    const compiledFn = this._compile(template);
    return await this._executeTemplate(compiledFn, data, '<string>');
  }

  /**
   * Compiles a template string into executable JavaScript code.
   * @private
   */
  _compile(template) {
    const { openTag, closeTag, echoTag } = this.options;
    let code = '';
    let cursor = 0;

    // Regex to match echo tags (<?= expr ?>) and code tags (<?jhs code ?>)
    const echoRegex = new RegExp(
      `${this._escapeRegex(echoTag)}([\\s\\S]*?)${this._escapeRegex(closeTag)}`,
      'g'
    );
    const codeRegex = new RegExp(
      `${this._escapeRegex(openTag)}([\\s\\S]*?)${this._escapeRegex(closeTag)}`,
      'g'
    );

    // First pass: convert echo tags to escaped output statements
    template = template.replace(echoRegex, (match, code) => {
      return `${openTag}__output += __escape(${code.trim()});${closeTag}`;
    });

    // Second pass: process code blocks
    let match;
    codeRegex.lastIndex = 0;

    while ((match = codeRegex.exec(template)) !== null) {
      // Append any HTML text before this tag
      if (match.index > cursor) {
        const htmlPart = template.slice(cursor, match.index);
        code += `__output += ${JSON.stringify(htmlPart)};\n`;
      }

      // Append the JavaScript block
      code += match[1] + '\n';
      cursor = codeRegex.lastIndex;
    }

    // Append remaining HTML after last tag
    if (cursor < template.length) {
      const htmlPart = template.slice(cursor);
      code += `__output += ${JSON.stringify(htmlPart)};\n`;
    }

    // Wrap everything in an IIFE so `return` works correctly
    const wrappedCode = `
(function() {
  let __output = "";

  // echo() — writes escaped output directly from code blocks.
  // __jhsEchoPart (injected into the sandbox context by the engine) unwraps
  // raw() sentinels and escapes everything else — mirroring the
  // wallermax-server Rust port.
  function echo(...args) {
    __output += args.map(__jhsEchoPart).join('');
  }

  ${code}

  return __output;
})()`;

    return wrappedCode;
  }

  /**
   * Executes a compiled template string inside a sandboxed VM context.
   * @private
   */
  async _executeTemplate(compiledCode, data, templatePath = '<unknown>') {
    const previousScriptName = this.options.scriptname;
    this.options.scriptname = templatePath;

    // Sentinel wrapper so __escape() can detect raw() values
    function RawString(str) { this.value = String(str); }

    // Build the sandbox context: data + allowed globals + helpers
    const escapeFn = this.options.autoEscape
      ? (val) => (val instanceof RawString ? val.value : this._escapeHtml(val))
      : (str) => (str instanceof RawString ? str.value : str);
    const context = {
      ...data,
      __escape: escapeFn,
      // echo() argument mapper: raw() sentinels pass through unescaped,
      // everything else goes through __escape(String(arg)) — the same
      // mechanism and quirks as the wallermax-server Rust port.
      __jhsEchoPart: (arg) =>
        arg instanceof RawString ? arg.value : escapeFn(String(arg)),
      require: this.require_filter,
      Buffer: Buffer,
      // include() — renders another template, inheriting parent data
      include: async (includePath, includeData = {}) => {
        return await this.render(includePath, { ...data, ...includeData });
      },
      escapeHtml: this._escapeHtml.bind(this),
      raw: (str) => new RawString(str), // Bypass auto-escape for trusted HTML
      JSON: JSON,
      console: console,         // Allow console.log inside templates for debugging
    };

    try {
      const script = new vm.Script(compiledCode, {
        filename: templatePath,
        timeout: 5000            // 5-second execution timeout
      });

      vm.createContext(context);
      const result = script.runInContext(context);

      this.options.scriptname = previousScriptName;
      return result;
    } catch (error) {
      this.options.scriptname = previousScriptName;
      throw new Error(`Template execution error (${templatePath}): ${error.message}`);
    }
  }

  /**
   * Filters require() calls inside templates, blocking banned modules.
   * @private
   */
  require_filter(pack) {
    if (this.options.banned_require.includes(pack)) {
      this.options.error = {
        error: true,
        file: this.options.scriptname,
        banned_require: pack
      };
      throw new Error(JSON.stringify(this.options.error));
    }
    return require(pack);
  }

  /**
   * Escapes HTML special characters to prevent XSS.
   * @private
   */
  _escapeHtml(unsafe) {
    if (unsafe === null || unsafe === undefined) return '';
    return String(unsafe)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#039;');
  }

  /**
   * Escapes special regex characters in a string.
   * @private
   */
  _escapeRegex(str) {
    return str.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  }

  /**
   * Clears the in-memory template cache.
   * Useful in development or after updating template files.
   */
  clearCache() {
    this.templateCache.clear();
  }

  /**
   * Clears Node.js require() cache entries matching a pattern.
   * Useful in development when .jhs modules change at runtime.
   * @param {string} [pattern='.jhs'] - String pattern to match cache keys
   */
  static clearRequireCache(pattern = '.jhs') {
    Object.keys(require.cache).forEach(key => {
      if (key.includes(pattern)) {
        delete require.cache[key];
      }
    });
  }
}

module.exports = JSTemplateEngine;
