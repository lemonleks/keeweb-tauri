module.exports = function (grunt) {
    grunt.registerTask('build-web-app', [
        'clean',
        'eslint',
        'copy:html',
        'copy:icons',
        'copy:manifest',
        'webpack:app',
        'inline',
        'htmlmin',
        'csp-hashes',
        'copy:content-dist',
        'string-replace:service-worker',
        'string-replace:update-manifest',
        'copy:dist-icons',
        'copy:dist-manifest'
    ]);

    grunt.registerTask('build-desktop', ['build-web-app', 'tauri-build']);

    grunt.registerTask('tauri-build', 'Bundle the desktop app with Tauri', () => {
        const result = require('child_process').spawnSync('npx tauri build', {
            stdio: 'inherit',
            shell: true
        });
        if (result.error) {
            grunt.log.error(result.error);
        }
        return result.status === 0;
    });

    grunt.registerTask('build-test', ['webpack:test']);
};
