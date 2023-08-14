import StartupManager from '/assets/js/modules/StartupManager/StartupManager.js'
import { GITHUB_PAGE, LINKEDIN_PAGE, RESUME_PDF_PATH, RESUME_HTML_PATH, CALCULATOR_PATH } from '/assets/js/utilities/Constants.js';

export class IconFactory {
    constructor(interactiveWindows, observable) {
        this.startupManager = new StartupManager(interactiveWindows, observable);
        this.iconActions = {
            'terminal-icon': this.startupManager.startTerminal.bind(this.startupManager),
            'resume-icon': this.startupManager.startPDFViewer.bind(this.startupManager, RESUME_PDF_PATH, RESUME_HTML_PATH),
            'github-icon': this.startupManager.startOpenPage.bind(this.startupManager, GITHUB_PAGE),
            'linkedin-icon': this.startupManager.startOpenPage.bind(this.startupManager, LINKEDIN_PAGE),
            'calculator-icon': this.startupManager.startHTMLViewer.bind(this.startupManager, CALCULATOR_PATH, 'Calculator'),
        };
    }

    initialize() {
        Object.entries(this.iconActions).forEach(([iconId, createWindowFunction]) => {
            const iconElement = document.querySelector(`#${iconId}`);
            iconElement.addEventListener('click', createWindowFunction);
        });
    }
}