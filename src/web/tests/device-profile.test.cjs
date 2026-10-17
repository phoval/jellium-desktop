const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const vm = require('node:vm');

// Exercise the injected shim's actual profile builder without a CEF runtime.
const source = readFileSync(join(__dirname, '..', 'native-shim.js'), 'utf8');
const profileCode = source.slice(
    source.indexOf('    const _deviceProfile ='),
    source.indexOf('    window.NativeShell.AppHost =')
);
assert.ok(profileCode.includes('function buildEffectiveDeviceProfile()'));

function createBuilder(videoCodecs = 'h264,hevc,av1,vp9') {
    const profile = {
        Name: 'test',
        DirectPlayProfiles: [
            { Type: 'Video', Container: 'mkv', VideoCodec: videoCodecs, AudioCodec: 'aac' },
            { Type: 'Audio', AudioCodec: 'aac' },
            { Type: 'Photo' }
        ],
        TranscodingProfiles: [
            { Type: 'Video', Container: 'mp4', VideoCodec: videoCodecs, AudioCodec: 'aac' },
            { Type: 'Video', Container: 'ts', VideoCodec: videoCodecs, AudioCodec: 'aac' },
            { Type: 'Audio' },
            { Type: 'Photo', Container: 'jpeg' }
        ]
    };
    const settings = { disabledVideoCodecs: [], forceTranscoding: false };
    const context = vm.createContext({ window: { jmpInfo: { settings: { transcode: settings } } } });
    vm.runInContext(profileCode.replace('__DEVICE_PROFILE_JSON__', JSON.stringify(profile)), context);
    return {
        profile,
        settings,
        build: () => JSON.parse(JSON.stringify(vm.runInContext('buildEffectiveDeviceProfile()', context)))
    };
}

test('defaults retain the full profile; exclusions affect video input and output only', () => {
    const { settings, profile, build } = createBuilder();
    assert.deepEqual(build(), profile);
    settings.disabledVideoCodecs = [' HEVC ', 'av1'];
    const result = build();
    assert.equal(result.DirectPlayProfiles[0].VideoCodec, 'h264,vp9');
    assert.equal(result.DirectPlayProfiles[0].AudioCodec, 'aac');
    assert.deepEqual(result.DirectPlayProfiles.slice(1), profile.DirectPlayProfiles.slice(1));
    for (const entry of result.TranscodingProfiles.filter(entry => entry.Type === 'Video')) {
        assert.equal(entry.VideoCodec, 'h264,vp9');
    }
    settings.disabledVideoCodecs = [];
    assert.deepEqual(build(), profile, 'filtering must not mutate the base profile');
});

test('excluding every available video codec removes video profiles', () => {
    const { settings, build } = createBuilder('hevc');
    settings.disabledVideoCodecs = ['hevc'];
    const result = build();
    assert.ok(result.DirectPlayProfiles.every(entry => entry.Type !== 'Video'));
    assert.ok(result.TranscodingProfiles.every(entry => entry.Type !== 'Video'));
});

test('forced transcoding retains photos and the TS target, then can be disabled again', () => {
    const { settings, profile, build } = createBuilder();
    settings.forceTranscoding = true;
    settings.disabledVideoCodecs = ['hevc'];
    const result = build();
    assert.deepEqual(result.DirectPlayProfiles, [{ Type: 'Photo' }]);
    const videoTargets = result.TranscodingProfiles.filter(entry => entry.Type === 'Video');
    assert.equal(videoTargets.length, 1);
    assert.equal(videoTargets[0].Container, 'ts');
    assert.equal(videoTargets[0].VideoCodec, 'h264,av1,vp9');
    settings.forceTranscoding = false;
    settings.disabledVideoCodecs = [];
    assert.deepEqual(build(), profile);
});
