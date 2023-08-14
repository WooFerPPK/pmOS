import { PromptHandler } from './PromptHandler.js';
import { Question } from './Question.js';
import { Conversation } from './Conversation.js';
import { Action } from './Action.js';
import StartupManager from '/assets/js/modules/StartupManager/StartupManager.js';
import { getDayOfWeek, formatDate, formatTime12Hour} from '/assets/js/utilities/DateTime.js';
import { initiateDownload } from '/assets/js/utilities/Download.js';
import { GITHUB_PAGE, LINKEDIN_PAGE, RESUME_TXT_PATH, RESUME_PDF_PATH, RESUME_HTML_PATH, RESUME_FILE_NAME } from '/assets/js/utilities/Constants.js';


export class Actions {
    constructor(terminal) {
        this.startupManager = new StartupManager(terminal.interactiveWindows, terminal.observable);

        this.help = new Action(() => {
            let commands = Object.keys(this);
            let basicTerminalOperations = ['help', 'clear', 'date', 'shutdown'];
            let resumeActions = ['resume', 'download resume', 'open resume', 'open html resume'];
            let socialMediaActions = ['github', 'linkedin'];
            
            let helpOutput = '';
        
            helpOutput += 'Basic Terminal Operations: ' + basicTerminalOperations.join(', ') + '<br></br>';
            helpOutput += 'Resume Actions: ' + resumeActions.join(', ') + '<br></br>';
            helpOutput += 'Social Media Actions: ' + socialMediaActions.join(', ') + '<br></br>';
            
            return helpOutput;
        });
        

        this.clear = new Action(() => {
            terminal.outputHandler.clearOutput();
            return '';
        });

        this.resume = this.createFetchAction(RESUME_TXT_PATH, (result) => {
            terminal.outputHandler.setTypeSpeed(-5);
            terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                if (!isRunning) {
                    terminal.outputHandler.setTypeSpeed(0); // originalTypeSpeed
                }
            });
            return result;
        });

        this['download resume'] = new Action(() => {
            initiateDownload(RESUME_PDF_PATH, RESUME_FILE_NAME);
            return `Initiating download of Paul Moscuzza's resume`;
        });

        this['open resume'] = new Action(() => {
            this.startupManager.startPDFViewer(RESUME_PDF_PATH, RESUME_HTML_PATH);
            return `Opening Paul Moscuzza's PDF resume`;
        });

        this['open html resume'] = new Action(() => {
            this.startupManager.startHTMLViewer(RESUME_HTML_PATH);
            return `Opening HTML version of Paul Moscuzza's resume`
        })
        
        this.github = this.createSocialMediaAction(GITHUB_PAGE, terminal);
        this.linkedin = this.createSocialMediaAction(LINKEDIN_PAGE, terminal);

        this.date = new Action(() => {
            const currentDate = new Date();
            return `${getDayOfWeek(currentDate)} ${formatDate(currentDate)} ${formatTime12Hour(currentDate)} `;
        });

        this.shutdown = this.createFetchAction('/assets/templates/text/shutdown.txt', (result) => {
            terminal.terminalEvents.removeCtrlCListener();

            terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                if (!isRunning) {
                    setTimeout(() => {
                        terminal.observable.notify("windowShutdown", { source: terminal, message: 'shutdown' });
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
                    const promptHandler = new PromptHandler(terminal, `Opening ${pageUrl} in a new tab`);
                    promptHandler.handle();
                    
                    this.startupManager.startOpenPage(pageUrl);
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
}
