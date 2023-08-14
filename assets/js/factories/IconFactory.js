import StartupManager from '/assets/js/modules/StartupManager/StartupManager.js'
import { GITHUB_PAGE, LINKEDIN_PAGE } from '/assets/js/utilities/Constants.js';

export class IconFactory {
    constructor(interactiveWindows, observable) {
        this.startupManager = new StartupManager(interactiveWindows, observable);
        this.iconActions = {
            'terminal-icon': this.startupManager.startTerminal.bind(this.startupManager),
            'resume-icon': this.startupManager.startPDFViewer.bind(this.startupManager, '/assets/templates/pdf/Paul-Moscuzza-Resume.pdf', '/assets/templates/html/Paul-Moscuzza-Resume.html'),
            'github-icon': this.startupManager.startOpenPage.bind(this.startupManager, GITHUB_PAGE),
            'linkedin-icon': this.startupManager.startOpenPage.bind(this.startupManager, LINKEDIN_PAGE)
        };
    }

    initialize() {
        Object.entries(this.iconActions).forEach(([iconId, createWindowFunction]) => {
            const iconElement = document.querySelector(`#${iconId}`);
            iconElement.addEventListener('click', createWindowFunction);
        });
    }
}