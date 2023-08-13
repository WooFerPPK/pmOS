import { PromptHandler } from './PromptHandler.js';
import { Question } from './Question.js';
import { Conversation } from './Conversation.js';
import { Action } from './Action.js';
import StartupManager from '/assets/js/modules/StartupManager/StartupManager.js';
import DateModule from '/assets/js/modules/DateModule/DateModule.js';

const GITHUB_PAGE = 'https://github.com/wooferppk';
const LINKEDIN_PAGE = 'https://www.linkedin.com/in/paulmoscuzza/';

export class Actions {
    constructor(terminal) {
        this.help = new Action(() => {
            return 'Supported commands: ' + Object.keys(this).join(', ');
        });

        this.clear = new Action(() => {
            terminal.outputHandler.clearOutput();
            return '';
        });

        this.resume = this.createFetchAction('/assets/templates/text/resume.txt', (result) => {
            terminal.outputHandler.setTypeSpeed(-5);
            terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                if (!isRunning) {
                    terminal.outputHandler.setTypeSpeed(0); // originalTypeSpeed
                }
            });
            return result;
        });

        this['download resume'] = new Action(() => {
            this.initiateDownload('/assets/templates/pdf/Paul-Moscuzza-Resume.pdf', 'Paul-Moscuzza-Resume.pdf');
            return 'Initiating download of the resume';
        });

        this['open resume'] = new Action(() => {
            this.startupManager = new StartupManager(terminal.observable, terminal.interactiveWindows);
            this.startupManager.startPDFViewer('/assets/templates/pdf/Paul-Moscuzza-Resume.pdf');
            return 'Opening resume window'
        });
        
        this.github = this.createSocialMediaAction(GITHUB_PAGE, terminal);
        this.linkedin = this.createSocialMediaAction(LINKEDIN_PAGE, terminal);

        this.date = new Action(() => {
            const dateModule = new DateModule();
            const currentDate = new Date();
            return `${dateModule.getDayOfWeek(currentDate)} ${dateModule.formatDate(currentDate)} ${dateModule.formatTime12Hour(currentDate)} `;
        });

        this.shutdown = this.createFetchAction('/assets/templates/text/shutdown.txt', (result) => {
            terminal.terminalEvents.removeCtrlCListener();

            terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                if (!isRunning) {
                    setTimeout(() => {
                        terminal.terminalUI.destroyTerminal();
                    }, 2000);
                }
            });

            return result;
        });
    }

    createFetchAction(url, onSuccess) {
        return new Action(async () => {
            try {
                const response = await fetch(url);
                if (!response.ok) {
                    throw new Error(`Failed to fetch from ${url}`);
                }
                const result = await response.text();
                return onSuccess(result);
            } catch (error) {
                return `Error: ${error.message}`;
            }
        });
    }

    createSocialMediaAction(pageUrl, terminal) {
        return new Action(() => {
            const QUESTION = `
                <a href="${pageUrl}" target="_blank">${pageUrl}</a>
                <br></br>
                Would you like me to redirect you to that page?
                <br></br>
                Type 'yes' or 'no'`;

            const question = new Question(QUESTION, {
                yes: new Action(() => {
                    window.open(pageUrl, '_blank');
                    const promptHandler = new PromptHandler(terminal, `Opening ${pageUrl} in a new tab`);
                    promptHandler.handle();
                }),
                no: new Action(() => {
                    const promptHandler = new PromptHandler(terminal, 'Okay, let me know if you need anything else!');
                    promptHandler.handle();
                })
            });

            const conversation = new Conversation(question, terminal, terminal.lastCommand());
            conversation.start();
            return '';
        });
    }

    initiateDownload(url, filename) {
        const a = document.createElement('a');
        a.href = url;
        a.download = filename;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
    }
}
