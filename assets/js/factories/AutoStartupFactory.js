import StartupManager from '../modules/StartupManager/StartupManager.js'

export default class AutoStartupFactory {
    constructor(interactiveWindows, observable) {
        this.startupManager = new StartupManager(interactiveWindows, observable);
    }

    initialize() {
        // Things to autostart on application load
        this.startupManager.startTerminal();
    }
}